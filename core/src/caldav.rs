//! CalDAV (RFC 4791) over HTTPS: finding an account's calendars (RFC 6764 and a provider list),
//! reading the events of a range of days, and writing one event at a time with ETags, so an
//! edit never overwrites a change made elsewhere in the meantime.

use chrono::{DateTime, Utc};
use reqwest::{Method, StatusCode, header};
use std::sync::Arc;
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
}

/// The HTTP client every request goes through: rustls with the system's certificates, no
/// redirects followed on its own (`send` follows them), and a 30 s limit.
pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("Signet/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .tls_backend_preconfigured(tls_config().map_err(|error| error.to_string())?)
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
    pub fn new(credentials: Credentials) -> Result<Self, Error> {
        Ok(Self {
            http: http_client().map_err(Error::Network)?,
            credentials,
        })
    }

    /// The Google tokens behind this client, when it signs in with Google.
    pub fn google_tokens(&self) -> Option<&crate::google::Tokens> {
        match &self.credentials {
            Credentials::Google(tokens) => Some(tokens),
            Credentials::Password { .. } => None,
        }
    }

    /// Sends a request, following redirects with the same method and body (a PROPFIND to
    /// `/.well-known/caldav` is answered with one). Credentials go only over HTTPS.
    async fn send(
        &self,
        method: Method,
        url: &Url,
        depth: Option<&str>,
        body: Option<String>,
        extra: &[(header::HeaderName, String)],
    ) -> Result<(Url, reqwest::Response), Error> {
        let mut url = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            if url.scheme() != "https" {
                return Err(Error::Unsupported("the calendar server must use HTTPS"));
            }
            let mut request = self.http.request(method.clone(), url.clone());
            request = match &self.credentials {
                Credentials::Password { username, password } => request.basic_auth(username, Some(password.as_str())),
                Credentials::Google(tokens) => {
                    let token = tokens.access().await.map_err(|error| match error {
                        crate::google::Error::SignInNeeded => Error::SignIn,
                        other => Error::Network(other.to_string()),
                    })?;
                    request.bearer_auth(token.as_str())
                }
            };
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
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|location| url.join(location).ok())
                    .ok_or(Error::Answer)?;
                url = location;
                continue;
            }
            return match response.status() {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(Error::Unauthorized),
                StatusCode::NOT_FOUND => Err(Error::NotFound),
                StatusCode::PRECONDITION_FAILED => Err(Error::Changed),
                status if status.is_success() => Ok((url, response)),
                status => Err(Error::Status(status)),
            };
        }
        Err(Error::Network("too many redirects".to_owned()))
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
            return Ok(home);
        }
        let principal = href_in(&document, DAV, "current-user-principal", &base).ok_or(Error::NoServer)?;
        let body = propfind(&["<c:calendar-home-set/>"]);
        let (base, text) = self.multistatus("PROPFIND", &principal, "0", body).await?;
        let document = roxmltree::Document::parse(&text).map_err(|_| Error::Answer)?;
        href_in(&document, CALDAV, "calendar-home-set", &base).ok_or(Error::NoServer)
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

fn tls_config() -> Result<rustls::ClientConfig, Error> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| Error::Network(error.to_string()))?;
    let config = match std::env::var_os(TEST_ROOTS_ENV) {
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

/// Where an account's calendars may be, most likely first: the configured address, a known
/// provider's server, or RFC 6764's well-known address at the mail domain.
pub fn candidates(address: &str, mail_host: Option<&str>, configured: Option<&str>) -> Result<Vec<Url>, Error> {
    if let Some(configured) = configured {
        return Url::parse(configured)
            .map(|url| vec![url])
            .map_err(|_| Error::Unsupported("the caldav address in Signet's config.toml isn't a URL"));
    }
    let domain = address.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase()).unwrap_or_default();
    let host = mail_host.unwrap_or_default().to_ascii_lowercase();
    let either = |suffixes: &[&str]| {
        suffixes
            .iter()
            .any(|s| domain == *s || domain.ends_with(&format!(".{s}")) || host.ends_with(s))
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
        return Ok(vec![Url::parse(url).expect("a valid URL")]);
    }
    let mut domains = vec![domain.clone()];
    if let Some((_, parent)) = host.split_once('.')
        && parent.contains('.')
        && !domains.contains(&parent.to_owned())
    {
        domains.push(parent.to_owned());
    }
    let mut urls = Vec::new();
    for domain in domains.iter().filter(|d| !d.is_empty()) {
        for form in ["https://{}/", "https://caldav.{}/", "https://dav.{}/"] {
            if let Ok(url) = Url::parse(&form.replace("{}", domain)) {
                urls.push(url);
            }
        }
    }
    if urls.is_empty() {
        return Err(Error::NoServer);
    }
    Ok(urls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_providers_and_well_known_guesses() {
        let fastmail = candidates("ada@example.com", Some("imap.fastmail.com"), None).unwrap();
        assert_eq!(fastmail[0].as_str(), "https://caldav.fastmail.com/");
        let icloud = candidates("ada@icloud.com", None, None).unwrap();
        assert_eq!(icloud[0].as_str(), "https://caldav.icloud.com/");
        let own = candidates("ada@example.org", Some("mail.example.net"), None).unwrap();
        assert_eq!(own[0].as_str(), "https://example.org/");
        assert!(own.iter().any(|url| url.as_str() == "https://example.net/"));
        let set = candidates("ada@gmail.com", None, Some("https://dav.example/cal/")).unwrap();
        assert_eq!(set[0].as_str(), "https://dav.example/cal/");
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
}
