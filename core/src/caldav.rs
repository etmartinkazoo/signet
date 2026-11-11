//! CalDAV (RFC 4791) over HTTPS: finding an account's calendars (RFC 6764 and a provider list),
//! reading the events of a range of days, and writing one event at a time with ETags, so an
//! edit never overwrites a change made elsewhere in the meantime.
//!
//! The password is often the account's mail password, so it goes only to servers the account
//! trusts: the configured or known one, or one the user allowed. It's sent once such a server
//! asks for it, never to a server a redirect names unless that server is trusted too.

use chrono::{DateTime, Utc};
use reqwest::{Method, StatusCode, header};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;
use zeroize::Zeroizing;

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
const DAV: &str = "DAV:";
const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
const APPLE: &str = "http://apple.com/ns/ical/";
const CALSERVER: &str = "http://calendarserver.org/ns/";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the server didn't accept the password")]
    Unauthorized,
    #[error("sign in with Google again")]
    SignIn,
    #[error("not found")]
    NotFound,
    #[error("the event changed on the server; refresh and try again")]
    Changed,
    #[error("the server answered {0}")]
    Status(StatusCode),
    #[error("couldn't reach the server: {0}")]
    Network(String),
    #[error("the server's answer couldn't be read")]
    Answer,
    #[error("no calendar server was found for this account")]
    NoServer,
    #[error("{0}")]
    Unsupported(&'static str),
    /// A trusted server sent the request on to one that isn't, or a calendar is on one.
    #[error("Signet won't send this account's password to {0}, which isn't its calendar server")]
    Elsewhere(String),
}

/// A calendar collection on the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteCalendar {
    pub url: Url,
    pub name: String,
    /// `#rrggbb`.
    pub color: Option<String>,
    pub ctag: Option<String>,
    pub read_only: bool,
}

/// One calendar object resource: an event and its moved occurrences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    pub url: Url,
    pub etag: Option<String>,
    pub ics: String,
}

/// When a PUT may write.
#[derive(Clone, Copy, Debug)]
pub enum Condition<'a> {
    /// Only if nothing has the name yet.
    Create,
    /// Only over exactly this version.
    Match(&'a str),
    /// Over whatever is there: for an event whose server sent no ETag.
    Any,
}

/// How requests prove who they're from.
pub enum Credentials {
    /// A password or app password, sent with HTTP Basic authentication.
    Password { username: String, password: Zeroizing<String> },
    /// An OAuth access token, renewed as it runs out (Google).
    Google(Arc<crate::google::Tokens>),
}

pub struct Client {
    http: reqwest::Client,
    credentials: Credentials,
    /// The origins the credentials may go to.
    trusted: Mutex<HashSet<url::Origin>>,
    /// Trusted origins that asked for Basic authentication, so the password goes with every
    /// request to them from now on.
    challenged: Mutex<HashSet<url::Origin>>,
}

/// The HTTP client every request goes through: rustls with the system's certificates, no
/// redirects followed on its own (`send` follows them), and a 30 s limit.
pub fn http_client() -> Result<reqwest::Client, String> {
    build_http(None)
}

fn build_http(roots: Option<&Path>) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("Signet/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .tls_backend_preconfigured(tls_config(roots).map_err(|error| error.to_string())?)
        .connect_timeout(TIMEOUT)
        .timeout(TIMEOUT)
        .build()
        .map_err(|error| error.to_string())
}

/// A request error in words: reqwest's own text is only "error sending request".
pub fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() { "timed out".to_owned() } else { chain(error) }
}

impl Client {
    /// A client that trusts no server yet: see [`Client::trust`].
    pub fn new(credentials: Credentials) -> Result<Self, Error> {
        Self::with_http(http_client().map_err(Error::Network)?, credentials)
    }

    /// A client that trusts only the certificates in a PEM file: for tests.
    pub fn with_roots(credentials: Credentials, roots: &Path) -> Result<Self, Error> {
        Self::with_http(build_http(Some(roots)).map_err(Error::Network)?, credentials)
    }

    fn with_http(http: reqwest::Client, credentials: Credentials) -> Result<Self, Error> {
        Ok(Self {
            http,
            credentials,
            trusted: Mutex::new(HashSet::new()),
            challenged: Mutex::new(HashSet::new()),
        })
    }

    /// Lets the credentials go to `url`'s origin (scheme, host and port).
    pub fn trust(&self, url: &Url) {
        if url.scheme() == "https" {
            lock(&self.trusted).insert(url.origin());
        }
    }

    pub fn trusts(&self, url: &Url) -> bool {
        lock(&self.trusted).contains(&url.origin())
    }

    fn challenged(&self, url: &Url) -> bool {
        lock(&self.challenged).contains(&url.origin())
    }

    /// The Google tokens behind this client, when it signs in with Google.
    pub fn google_tokens(&self) -> Option<&crate::google::Tokens> {
        match &self.credentials {
            Credentials::Google(tokens) => Some(tokens),
            Credentials::Password { .. } => None,
        }
    }

    /// Sends a request with the account's credentials and reads the answer's status.
    async fn send(
        &self,
        method: Method,
        url: &Url,
        depth: Option<&str>,
        body: Option<String>,
        extra: &[(header::HeaderName, String)],
    ) -> Result<(Url, reqwest::Response), Error> {
        let (url, response) = self.fetch(method, url, depth, body, extra, true).await?;
        match response.status() {
            StatusCode::UNAUTHORIZED if !self.trusts(&url) => Err(Error::Elsewhere(host_of(&url))),
            StatusCode::UNAUTHORIZED
                if matches!(self.credentials, Credentials::Password { .. }) && !self.challenged(&url) =>
            {
                Err(Error::Unsupported("the calendar server asks for a kind of sign-in Signet doesn't support"))
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(Error::Unauthorized),
            StatusCode::NOT_FOUND => Err(Error::NotFound),
            StatusCode::PRECONDITION_FAILED => Err(Error::Changed),
            status if status.is_success() => Ok((url, response)),
            status => Err(Error::Status(status)),
        }
    }

    /// Sends a request, following redirects with the same method and body (a PROPFIND to
    /// `/.well-known/caldav` is answered with one), and returns the last answer whatever its
    /// status. With `authenticate`, credentials go to trusted origins only: the password once
    /// the server asks for it with a Basic challenge, Google's token up front. A redirect from a
    /// trusted origin to one that isn't trusted is refused, not followed.
    async fn fetch(
        &self,
        method: Method,
        url: &Url,
        depth: Option<&str>,
        body: Option<String>,
        extra: &[(header::HeaderName, String)],
        authenticate: bool,
    ) -> Result<(Url, reqwest::Response), Error> {
        let mut url = url.clone();
        let mut redirects = 0;
        loop {
            if url.scheme() != "https" {
                return Err(Error::Unsupported("the calendar server must use HTTPS"));
            }
            let trusted = authenticate && self.trusts(&url);
            let mut request = self.http.request(method.clone(), url.clone());
            if trusted {
                request = match &self.credentials {
                    Credentials::Password { username, password } if self.challenged(&url) => {
                        request.basic_auth(username, Some(password.as_str()))
                    }
                    Credentials::Password { .. } => request,
                    Credentials::Google(tokens) => {
                        let token = tokens.access().await.map_err(|error| match error {
                            crate::google::Error::SignInNeeded => Error::SignIn,
                            other => Error::Network(other.to_string()),
                        })?;
                        request.bearer_auth(token.as_str())
                    }
                };
            }
            if let Some(depth) = depth {
                request = request.header("Depth", depth);
            }
            for (name, value) in extra {
                request = request.header(name, value);
            }
            if let Some(body) = &body {
                let kind = if method == Method::PUT { "text/calendar; charset=utf-8" } else { "application/xml; charset=utf-8" };
                request = request.header(header::CONTENT_TYPE, kind).body(body.clone());
            }
            let response = request.send().await.map_err(|error| Error::Network(describe(&error)))?;
            if trusted
                && response.status() == StatusCode::UNAUTHORIZED
                && matches!(self.credentials, Credentials::Password { .. })
                && !self.challenged(&url)
                && asks_for_basic(&response)
            {
                lock(&self.challenged).insert(url.origin());
                continue;
            }
            if response.status().is_redirection() {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(Error::Network("too many redirects".to_owned()));
                }
                let next = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|location| url.join(location).ok())
                    .ok_or(Error::Answer)?;
                if trusted && next.origin() != url.origin() && !self.trusts(&next) {
                    return Err(Error::Elsewhere(host_of(&next)));
                }
                url = next;
                continue;
            }
            return Ok((url, response));
        }
    }

    /// Looks for a calendar server at a mail domain's `/.well-known/caldav` (RFC 6764) without
    /// sending any credentials. Returns where the redirects ended, if a server there asked to
    /// sign in or answered as WebDAV: one to offer the user, not to send the password to unasked.
    pub async fn probe(&self, domain: &str) -> Result<Url, Error> {
        let start = Url::parse(&format!("https://{domain}/.well-known/caldav")).map_err(|_| Error::NoServer)?;
        let method = Method::from_bytes(b"PROPFIND").expect("a valid method");
        let body = propfind(&["<d:current-user-principal/>"]);
        let (url, response) = self.fetch(method, &start, Some("0"), Some(body), &[], false).await?;
        match response.status() {
            StatusCode::MULTI_STATUS => Ok(url),
            StatusCode::UNAUTHORIZED if asks_for_basic(&response) => Ok(url),
            _ => Err(Error::NoServer),
        }
    }

    async fn multistatus(&self, method: &str, url: &Url, depth: &str, body: String) -> Result<(Url, String), Error> {
        let method = Method::from_bytes(method.as_bytes()).expect("a valid method");
        let (url, response) = self.send(method, url, Some(depth), Some(body), &[]).await?;
        let text = response.text().await.map_err(|_| Error::Answer)?;
        Ok((url, text))
    }

    /// The calendar home of the account behind `start`: a server root, a `/.well-known/caldav`
    /// address, a principal or the home itself.
    pub async fn discover(&self, start: &Url) -> Result<Url, Error> {
        let body = propfind(&["<d:current-user-principal/>", "<c:calendar-home-set/>"]);
        let (base, text) = match self.multistatus("PROPFIND", start, "0", body.clone()).await {
            Err(Error::NotFound | Error::Status(_)) if start.path() == "/" => {
                let known = start.join("/.well-known/caldav").map_err(|_| Error::Answer)?;
                self.multistatus("PROPFIND", &known, "0", body).await?
            }
            other => other?,
        };
        let document = roxmltree::Document::parse(&text).map_err(|_| Error::Answer)?;
        if let Some(home) = href_in(&document, CALDAV, "calendar-home-set", &base) {
            return Ok(self.named_by(&base, home));
        }
        let principal = href_in(&document, DAV, "current-user-principal", &base).ok_or(Error::NoServer)?;
        let principal = self.named_by(&base, principal);
        let body = propfind(&["<c:calendar-home-set/>"]);
        let (base, text) = self.multistatus("PROPFIND", &principal, "0", body).await?;
        let document = roxmltree::Document::parse(&text).map_err(|_| Error::Answer)?;
        let home = href_in(&document, CALDAV, "calendar-home-set", &base).ok_or(Error::NoServer)?;
        Ok(self.named_by(&base, home))
    }

    /// An address a server's answer named: trusted when that server is, as iCloud names a home
    /// on another host. The server already holds the credentials, so this tells no one new.
    fn named_by(&self, server: &Url, named: Url) -> Url {
        if self.trusts(server) {
            self.trust(&named);
        }
        named
    }

    /// The event calendars in a calendar home.
    pub async fn calendars(&self, home: &Url) -> Result<Vec<RemoteCalendar>, Error> {
        let body = propfind(&[
            "<d:resourcetype/>",
            "<d:displayname/>",
            "<a:calendar-color/>",
            "<cs:getctag/>",
            "<d:sync-token/>",
            "<c:supported-calendar-component-set/>",
            "<d:current-user-privilege-set/>",
        ]);
        let (base, text) = self.multistatus("PROPFIND", home, "1", body).await?;
        parse_calendars(&text, &base)
    }

    /// The calendar's events with an occurrence in `from..to`; the server expands series to
    /// decide, and sends each object whole.
    pub async fn events(&self, calendar: &Url, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<Resource>, Error> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT">
    <c:time-range start="{}" end="{}"/>
  </c:comp-filter></c:comp-filter></c:filter>
</c:calendar-query>"#,
            from.format("%Y%m%dT%H%M%SZ"),
            to.format("%Y%m%dT%H%M%SZ")
        );
        let (base, text) = self.multistatus("REPORT", calendar, "1", body).await?;
        parse_events(&text, &base)
    }
    /// Writes an event under `condition`. Returns the new ETag when the server says it.
    pub async fn put(&self, url: &Url, ics: &str, condition: Condition<'_>) -> Result<Option<String>, Error> {
        let condition: Vec<_> = match condition {
            Condition::Create => vec![(header::IF_NONE_MATCH, "*".to_owned())],
            Condition::Match(etag) => vec![(header::IF_MATCH, etag.to_owned())],
            Condition::Any => Vec::new(),
        };
        let (_, response) = self.send(Method::PUT, url, None, Some(ics.to_owned()), &condition).await?;
        Ok(response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned))
    }

    pub async fn delete(&self, url: &Url, etag: Option<&str>) -> Result<(), Error> {
        let condition: Vec<_> = etag.map(|etag| (header::IF_MATCH, etag.to_owned())).into_iter().collect();
        match self.send(Method::DELETE, url, None, None, &condition).await {
            Ok(_) | Err(Error::NotFound) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// The event calendars in a PROPFIND answer for a calendar home.
pub fn parse_calendars(text: &str, base: &Url) -> Result<Vec<RemoteCalendar>, Error> {
let document = roxmltree::Document::parse(text).map_err(|_| Error::Answer)?;
let mut found = Vec::new();
for response in responses(&document) {
    let Some(url) = response_url(response, base) else { continue };
    let props = ok_props(response);
    let is_calendar = props.iter().any(|prop| {
            child(*prop, DAV, "resourcetype").is_some_and(|kind| child(kind, CALDAV, "calendar").is_some())
        });
        if !is_calendar {
            continue;
        }
        let components: Vec<String> = props
            .iter()
            .filter_map(|prop| child(*prop, CALDAV, "supported-calendar-component-set"))
            .flat_map(|set| set.children().filter_map(|comp| comp.attribute("name").map(str::to_ascii_uppercase)))
            .collect();
        if !components.is_empty() && !components.iter().any(|name| name == "VEVENT") {
            continue;
        }
        let text_of = |ns: &str, name: &str| {
            props
                .iter()
                .find_map(|prop| child(*prop, ns, name))
                .and_then(|node| node.text())
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty())
        };
        let privileges: Vec<&str> = props
            .iter()
            .filter_map(|prop| child(*prop, DAV, "current-user-privilege-set"))
            .flat_map(|set| set.descendants().filter(|n| n.is_element()).map(|n| n.tag_name().name()))
            .collect();
        let read_only = !privileges.is_empty()
            && !privileges.iter().any(|name| matches!(*name, "write" | "write-content" | "all"));
        let name = text_of(DAV, "displayname").unwrap_or_else(|| {
            url.path_segments()
                .and_then(|mut segments| segments.rfind(|s| !s.is_empty()).map(str::to_owned))
                .unwrap_or_else(|| "Calendar".to_owned())
        });
        found.push(RemoteCalendar {
            name,
            color: text_of(APPLE, "calendar-color").and_then(|color| normalise_color(&color)),
            ctag: text_of(CALSERVER, "getctag").or_else(|| text_of(DAV, "sync-token")),
            read_only,
            url,
        });
    }
    Ok(found)
}

/// The events in a calendar-query answer.
pub fn parse_events(text: &str, base: &Url) -> Result<Vec<Resource>, Error> {
    let document = roxmltree::Document::parse(text).map_err(|_| Error::Answer)?;
    let mut found = Vec::new();
    for response in responses(&document) {
        let Some(url) = response_url(response, base) else { continue };
        let props = ok_props(response);
        let ics = props
            .iter()
            .find_map(|prop| child(*prop, CALDAV, "calendar-data"))
            .and_then(|node| node.text())
            .map(str::to_owned);
        let etag = props
            .iter()
            .find_map(|prop| child(*prop, DAV, "getetag"))
            .and_then(|node| node.text())
            .map(|etag| etag.trim().to_owned());
        if let Some(ics) = ics {
            found.push(Resource { url, etag, ics });
        }
    }
    Ok(found)
}

/// Names a PEM file whose certificates are trusted instead of the system's: for tests against a
/// local server only.
pub const TEST_ROOTS_ENV: &str = "SIGNET_TEST_ROOTS";

fn tls_config(roots: Option<&Path>) -> Result<rustls::ClientConfig, Error> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| Error::Network(error.to_string()))?;
    let roots = roots.map(Path::to_path_buf).or_else(|| std::env::var_os(TEST_ROOTS_ENV).map(Into::into));
    let config = match roots {
        Some(path) => {
            let mut roots = rustls::RootCertStore::empty();
            for cert in CertificateDer::pem_file_iter(path).map_err(|e| Error::Network(e.to_string()))? {
                roots
                    .add(cert.map_err(|e| Error::Network(e.to_string()))?)
                    .map_err(|e| Error::Network(e.to_string()))?;
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        None => builder
            .with_platform_verifier()
            .map_err(|error| Error::Network(error.to_string()))?
            .with_no_client_auth(),
    };
    Ok(config)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn host_of(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        _ => url.to_string(),
    }
}

/// Whether a 401 answer offers Basic authentication among its challenges.
fn asks_for_basic(response: &reqwest::Response) -> bool {
    response.headers().get_all(header::WWW_AUTHENTICATE).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split([',', ' ', '\t'])
                .any(|token| token.eq_ignore_ascii_case("basic"))
        })
    })
}

/// An error with its causes: reqwest's own text is only "error sending request".
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn propfind(props: &[&str]) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:a="http://apple.com/ns/ical/" xmlns:cs="http://calendarserver.org/ns/">
  <d:prop>{}</d:prop>
</d:propfind>"#,
        props.concat()
    )
}

fn child<'a, 'input>(node: roxmltree::Node<'a, 'input>, ns: &str, name: &str) -> Option<roxmltree::Node<'a, 'input>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name && n.tag_name().namespace() == Some(ns))
}

fn responses<'a, 'input>(document: &'a roxmltree::Document<'input>) -> impl Iterator<Item = roxmltree::Node<'a, 'input>> {
    document
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "response" && n.tag_name().namespace() == Some(DAV))
}

fn response_url(response: roxmltree::Node, base: &Url) -> Option<Url> {
    let href = child(response, DAV, "href")?.text()?.trim();
    base.join(href).ok()
}

/// The `prop` elements of a response's `propstat`s with a 200 status.
fn ok_props<'a, 'input>(response: roxmltree::Node<'a, 'input>) -> Vec<roxmltree::Node<'a, 'input>> {
    response
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "propstat")
        .filter(|propstat| {
            child(*propstat, DAV, "status")
                .and_then(|status| status.text())
                .is_none_or(|status| status.contains(" 200"))
        })
        .filter_map(|propstat| child(propstat, DAV, "prop"))
        .collect()
}

/// The first href inside a property anywhere in the answer.
fn href_in(document: &roxmltree::Document, ns: &str, name: &str, base: &Url) -> Option<Url> {
    let property = document
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == name && n.tag_name().namespace() == Some(ns))?;
    let href = child(property, DAV, "href")?.text()?.trim();
    base.join(href).ok()
}

/// `#RRGGBB` from Apple's `#RRGGBBAA` or `#RGB`.
fn normalise_color(color: &str) -> Option<String> {
    let hex = color.trim().trim_start_matches('#');
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        6 | 8 => Some(format!("#{}", hex[..6].to_ascii_lowercase())),
        3 => Some(format!("#{}", hex.chars().flat_map(|c| [c, c]).collect::<String>().to_ascii_lowercase())),
        _ => None,
    }
}

/// Where to look for an account's calendars.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// The configured address, or a known provider's server: trusted with the credentials.
    Server(Url),
    /// The mail domain, whose `/.well-known/caldav` may name a server ([`Client::probe`]). The
    /// user is asked before the password goes there.
    Domain(String),
}

/// Where an account's calendars are: the configured address, a known provider's server, or else
/// whatever the address's domain names.
pub fn locate(address: &str, mail_host: Option<&str>, configured: Option<&str>) -> Result<Location, Error> {
    if let Some(configured) = configured {
        return Url::parse(configured)
            .map(Location::Server)
            .map_err(|_| Error::Unsupported("the caldav address in Signet's config.toml isn't a URL"));
    }
    let domain = address.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase()).unwrap_or_default();
    let host = mail_host.unwrap_or_default().to_ascii_lowercase();
    let either = |suffixes: &[&str]| {
        suffixes
            .iter()
            .any(|s| domain == *s || domain.ends_with(&format!(".{s}")) || host == *s || host.ends_with(&format!(".{s}")))
    };
    if either(&["outlook.com", "hotmail.com", "live.com", "msn.com", "office365.com"]) {
        return Err(Error::Unsupported("Outlook calendars need Microsoft Graph, which Signet doesn't support yet"));
    }
    let known: &[(&[&str], &str)] = &[
        (&["icloud.com", "me.com", "mac.com", "mail.me.com"], "https://caldav.icloud.com/"),
        (&["fastmail.com", "fastmail.fm", "messagingengine.com"], "https://caldav.fastmail.com/"),
        (&["yahoo.com", "ymail.com", "rocketmail.com"], "https://caldav.calendar.yahoo.com/"),
        (&["aol.com"], "https://caldav.aol.com/"),
        (&["mailbox.org"], "https://dav.mailbox.org/"),
        (&["posteo.de", "posteo.net"], "https://posteo.de:8443/"),
        (&["gmx.net", "gmx.de", "gmx.at", "gmx.ch"], "https://caldav.gmx.net/"),
        (&["gmx.com"], "https://caldav.gmx.com/"),
        (&["web.de"], "https://caldav.web.de/"),
        (&["zoho.com", "zohomail.com"], "https://calendar.zoho.com/"),
        (&["yandex.ru", "yandex.com"], "https://caldav.yandex.ru/"),
    ];
    if let Some((_, url)) = known.iter().find(|(suffixes, _)| either(suffixes)) {
        return Ok(Location::Server(Url::parse(url).expect("a valid URL")));
    }
    if domain.is_empty() || Url::parse(&format!("https://{domain}/")).is_err() {
        return Err(Error::NoServer);
    }
    Ok(Location::Domain(domain))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_providers_configured_servers_and_domains() {
        let server = |url: &str| Location::Server(Url::parse(url).unwrap());
        let fastmail = locate("ada@example.com", Some("imap.fastmail.com"), None).unwrap();
        assert_eq!(fastmail, server("https://caldav.fastmail.com/"));
        assert_eq!(locate("ada@icloud.com", None, None).unwrap(), server("https://caldav.icloud.com/"));
        let set = locate("ada@gmail.com", None, Some("https://dav.example/cal/")).unwrap();
        assert_eq!(set, server("https://dav.example/cal/"));
        // No guesses at the website, its subdomains or the mail server's domain: only the
        // address's domain, to look up its well-known address without the password.
        let own = locate("ada@Example.org", Some("mail.example.net"), None).unwrap();
        assert_eq!(own, Location::Domain("example.org".to_owned()));
        // A mail host merely ending in a provider's name isn't that provider.
        let lookalike = locate("ada@example.org", Some("mail.notfastmail.com"), None).unwrap();
        assert_eq!(lookalike, Location::Domain("example.org".to_owned()));
        assert!(matches!(locate("nobody", None, None), Err(Error::NoServer)));
    }

    #[test]
    fn colours() {
        assert_eq!(normalise_color("#FF2968FF").as_deref(), Some("#ff2968"));
        assert_eq!(normalise_color("#abc").as_deref(), Some("#aabbcc"));
        assert_eq!(normalise_color("red"), None);
    }

    #[test]
    fn reads_a_calendar_listing() {
        let xml = r##"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:a="http://apple.com/ns/ical/" xmlns:cs="http://calendarserver.org/ns/">
 <d:response><d:href>/dav/cal/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/dav/cal/work/</d:href><d:propstat><d:prop>
   <d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:displayname>Work</d:displayname>
   <a:calendar-color>#3A87ADFF</a:calendar-color><cs:getctag>42</cs:getctag>
   <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
 </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 <d:propstat><d:prop><d:sync-token/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response>
 <d:response><d:href>/dav/cal/tasks/</d:href><d:propstat><d:prop>
   <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
   <c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set>
 </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"##;
        let base = Url::parse("https://dav.example/dav/cal/").unwrap();
        let calendars = parse_calendars(xml, &base).unwrap();
        assert_eq!(calendars.len(), 1, "the VTODO list and the home itself are left out");
        let work = &calendars[0];
        assert_eq!(work.url.as_str(), "https://dav.example/dav/cal/work/");
        assert_eq!(work.name, "Work");
        assert_eq!(work.color.as_deref(), Some("#3a87ad"));
        assert_eq!(work.ctag.as_deref(), Some("42"));
        assert!(!work.read_only);
    }

    #[test]
    fn reads_events_with_etags() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
 <d:response><d:href>/dav/cal/work/a%20b.ics</d:href><d:propstat><d:prop>
  <d:getetag>"e1"</d:getetag><c:calendar-data>BEGIN:VCALENDAR&#13;
END:VCALENDAR&#13;
</c:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"#;
        let base = Url::parse("https://dav.example/dav/cal/work/").unwrap();
        let events = parse_events(xml, &base).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].url.as_str(), "https://dav.example/dav/cal/work/a%20b.ics");
        assert_eq!(events[0].etag.as_deref(), Some("\"e1\""));
        assert!(events[0].ics.starts_with("BEGIN:VCALENDAR"));
    }

    /// A local HTTPS server for the tests, on 127.0.0.1 with the certificate in `testdata`.
    mod server {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        #[derive(Clone, Debug)]
        pub struct Seen {
            pub method: String,
            pub path: String,
            pub authorization: Option<String>,
        }

        pub struct Server {
            pub url: url::Url,
            pub seen: Arc<Mutex<Vec<Seen>>>,
        }

        impl Server {
            pub fn seen(&self) -> Vec<Seen> {
                self.seen.lock().unwrap().clone()
            }
        }

        /// Answers each request with `answer(request)`: the status line's code and reason, extra
        /// header lines, and a body.
        pub async fn start(answer: impl Fn(&Seen) -> (&'static str, Vec<String>, String) + Send + Sync + 'static) -> Server {
            use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
            let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/testdata/");
            let certs = CertificateDer::pem_file_iter(format!("{dir}server.pem")).unwrap().map(Result::unwrap).collect();
            let key = PrivateKeyDer::from_pem_file(format!("{dir}server.key")).unwrap();
            let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = url::Url::parse(&format!("https://127.0.0.1:{}/", listener.local_addr().unwrap().port())).unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&seen);
            let answer = Arc::new(answer);
            tokio::spawn(async move {
                loop {
                    let Ok((tcp, _)) = listener.accept().await else { return };
                    let (acceptor, log, answer) = (acceptor.clone(), Arc::clone(&log), Arc::clone(&answer));
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
                        let mut stream = BufReader::new(tls);
                        loop {
                            let mut line = String::new();
                            if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            let mut parts = line.split_whitespace();
                            let (method, path) = (parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("").to_owned());
                            let (mut length, mut authorization) = (0, None);
                            loop {
                                let mut header = String::new();
                                stream.read_line(&mut header).await.unwrap();
                                let header = header.trim_end();
                                if header.is_empty() {
                                    break;
                                }
                                let (name, value) = header.split_once(':').unwrap();
                                match name.to_ascii_lowercase().as_str() {
                                    "content-length" => length = value.trim().parse().unwrap(),
                                    "authorization" => authorization = Some(value.trim().to_owned()),
                                    _ => {}
                                }
                            }
                            let mut body = vec![0; length];
                            stream.read_exact(&mut body).await.unwrap();
                            let request = Seen { method, path, authorization };
                            log.lock().unwrap().push(request.clone());
                            let (status, headers, body) = answer(&request);
                            let mut reply = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
                            for header in headers {
                                reply.push_str(&header);
                                reply.push_str("\r\n");
                            }
                            reply.push_str("\r\n");
                            reply.push_str(&body);
                            if stream.get_mut().write_all(reply.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            Server { url, seen }
        }
    }

    fn password_client() -> Client {
        let roots = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/testdata/ca.pem"));
        let credentials = Credentials::Password { username: "ada".into(), password: Zeroizing::new("secret".into()) };
        Client::with_roots(credentials, roots).unwrap()
    }

    const HOME: &str = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/</d:href>
<d:propstat><d:prop><c:calendar-home-set><d:href>/cal/ada/</d:href></c:calendar-home-set></d:prop>
<d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;

    /// A CalDAV server that asks for Basic authentication and then answers with a home.
    fn caldav(request: &server::Seen) -> (&'static str, Vec<String>, String) {
        match &request.authorization {
            None => ("401 Unauthorized", vec![r#"WWW-Authenticate: Basic realm="cal""#.into()], String::new()),
            Some(_) => ("207 Multi-Status", vec!["Content-Type: application/xml".into()], HOME.to_owned()),
        }
    }

    #[tokio::test]
    async fn the_password_waits_for_a_basic_challenge() {
        let server = server::start(caldav).await;
        let client = password_client();
        client.trust(&server.url);
        let home = client.discover(&server.url).await.unwrap();
        assert_eq!(home.path(), "/cal/ada/");
        let seen = server.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].authorization, None, "the first request carries no password");
        assert_eq!(seen[1].authorization.as_deref(), Some("Basic YWRhOnNlY3JldA=="));
        client.discover(&server.url).await.unwrap();
        assert_eq!(server.seen().len(), 3, "once asked, the password goes along with no second challenge");
    }

    #[tokio::test]
    async fn a_redirect_to_another_host_gets_no_password() {
        let elsewhere = server::start(caldav).await;
        let target = elsewhere.url.join("/dav/").unwrap().to_string();
        let trusted = server::start(move |request| match &request.authorization {
            None => ("401 Unauthorized", vec![r#"WWW-Authenticate: Basic realm="cal""#.into()], String::new()),
            Some(_) => ("301 Moved Permanently", vec![format!("Location: {target}")], String::new()),
        })
        .await;
        let client = password_client();
        client.trust(&trusted.url);
        let result = client.discover(&trusted.url.join("/dav/").unwrap()).await;
        assert!(matches!(result, Err(Error::Elsewhere(_))), "{result:?}");
        assert!(trusted.seen().iter().any(|seen| seen.authorization.is_some()));
        assert!(elsewhere.seen().iter().all(|seen| seen.authorization.is_none()), "{:?}", elsewhere.seen());
    }

    #[tokio::test]
    async fn a_server_that_never_asks_never_gets_the_password() {
        let server = server::start(|_| ("404 Not Found", Vec::new(), String::new())).await;
        let client = password_client();
        client.trust(&server.url);
        assert!(client.discover(&server.url).await.is_err());
        let seen = server.seen();
        assert_eq!(seen.len(), 2, "the root, then /.well-known/caldav: {seen:?}");
        assert!(seen.iter().all(|seen| seen.method == "PROPFIND"));
        assert!(seen.iter().all(|seen| seen.authorization.is_none()));
    }

    #[tokio::test]
    async fn an_untrusted_server_gets_no_password_even_when_it_asks() {
        let server = server::start(caldav).await;
        let client = password_client();
        let result = client.discover(&server.url.join("/dav/").unwrap()).await;
        assert!(matches!(result, Err(Error::Elsewhere(_))), "{result:?}");
        assert!(server.seen().iter().all(|seen| seen.authorization.is_none()));
    }

    #[tokio::test]
    async fn probing_a_domain_follows_redirects_without_the_password() {
        let calendars = server::start(caldav).await;
        let target = calendars.url.join("/remote.php/dav/").unwrap().to_string();
        let website = server::start(move |_| ("301 Moved Permanently", vec![format!("Location: {target}")], String::new())).await;
        let client = password_client();
        let domain = format!("127.0.0.1:{}", website.url.port().unwrap());
        let found = client.probe(&domain).await.unwrap();
        assert_eq!(found.as_str(), calendars.url.join("/remote.php/dav/").unwrap().as_str());
        assert_eq!(website.seen()[0].path, "/.well-known/caldav");
        assert!(website.seen().iter().chain(&calendars.seen()).all(|seen| seen.authorization.is_none()));
        assert!(!client.trusts(&found), "found isn't trusted until the user allows it");

        let nothing = server::start(|_| ("404 Not Found", Vec::new(), String::new())).await;
        let domain = format!("127.0.0.1:{}", nothing.url.port().unwrap());
        assert!(matches!(client.probe(&domain).await, Err(Error::NoServer)));
    }

    #[tokio::test]
    async fn a_trusted_server_may_name_a_home_on_another_host() {
        let other = server::start(caldav).await;
        let named = other.url.join("/cal/ada/").unwrap().to_string();
        let trusted = server::start(move |request| match &request.authorization {
            None => ("401 Unauthorized", vec![r#"WWW-Authenticate: Basic realm="cal""#.into()], String::new()),
            Some(_) => ("207 Multi-Status", Vec::new(), HOME.replace("/cal/ada/", &named)),
        })
        .await;
        let client = password_client();
        client.trust(&trusted.url);
        let home = client.discover(&trusted.url).await.unwrap();
        assert_eq!(home.origin(), other.url.origin());
        assert!(client.trusts(&home), "as iCloud does: the trusted server named it");
    }
}
