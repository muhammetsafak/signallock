//! The only HTTP code in the app (ADR-015).
//!
//! Rules enforced here, for every request the updater ever makes:
//!
//! * HTTPS only, default port only, no credentials in the URL.
//! * The host must be on the allowlist ([`ALLOWED_HOSTS`]).
//! * `ureq` follows no redirect by itself. Redirects are followed by hand, at
//!   most [`MAX_REDIRECTS`], and every hop is checked against the allowlist
//!   *before* a connection is made.
//! * Connect, read and total-body timeouts, and a hard cap on the body size of
//!   every response ([`Resource::max_bytes`]).
//! * Headers: `User-Agent: SignalLock/<version>` and `Accept`. No cookie, no
//!   token, no other identifier.
//! * **System proxy (user decision recorded in ADR-015).** The standard proxy
//!   variables (`ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY`) are
//!   honoured on purpose: see [`system_proxy`]. A proxy only changes the route,
//!   never the target: the allowlist check runs on the request URL *before* a
//!   proxy is involved, so the `CONNECT` target is always an allowlisted
//!   `host:443`, and TLS is negotiated end to end with that host (Mozilla roots,
//!   certificate verification on, see [`build_agent`]); the proxy sees only the
//!   encrypted tunnel.
//!
//! The allowlist and the API base URL can be replaced only by test builds
//! (`#[cfg(test)]`); a release build has no code path that relaxes them.

use std::fmt;
use std::io::{self, Read};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ureq::Agent;
use ureq::http::{HeaderMap, Uri};

use super::UpdateError;

/// Hosts the updater may talk to (ADR-015). `objects.githubusercontent.com` is
/// the earlier asset host; `release-assets.githubusercontent.com` is where
/// `github.com/.../releases/download/...` redirects to today.
pub const ALLOWED_HOSTS: [&str; 4] = [
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// Redirects followed by hand before giving up.
pub const MAX_REDIRECTS: usize = 5;

/// Cap on the `releases/latest` JSON.
pub const API_MAX_BYTES: u64 = 1024 * 1024;
/// Cap on a release package.
pub const PACKAGE_MAX_BYTES: u64 = 200 * 1024 * 1024;
/// Cap on a `.sha256` file.
pub const CHECKSUM_MAX_BYTES: u64 = 1024;

const API_BASE: &str = "https://api.github.com";
const REPOSITORY: &str = "muhammetsafak/signallock";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(20);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const SMALL_BODY_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
const PACKAGE_BODY_TOTAL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// What is being fetched. It decides the `Accept` header, the size cap and the
/// total-body timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// `releases/latest` JSON.
    ApiJson,
    /// `<asset>.sha256`.
    Checksum,
    /// The release package itself.
    Package,
}

impl Resource {
    pub const fn max_bytes(self) -> u64 {
        match self {
            Self::ApiJson => API_MAX_BYTES,
            Self::Checksum => CHECKSUM_MAX_BYTES,
            Self::Package => PACKAGE_MAX_BYTES,
        }
    }

    const fn accept(self) -> &'static str {
        match self {
            Self::ApiJson => "application/vnd.github+json",
            Self::Checksum | Self::Package => "application/octet-stream",
        }
    }
}

/// Which URLs are acceptable. Production is always strict; only test builds can
/// build a lenient policy.
#[derive(Debug, Clone)]
struct Policy {
    allowed_hosts: Vec<String>,
    /// `true`: HTTPS and the default port only.
    strict: bool,
}

impl Policy {
    fn production() -> Self {
        Self {
            allowed_hosts: ALLOWED_HOSTS.iter().map(|h| (*h).to_owned()).collect(),
            strict: true,
        }
    }

    /// Plain HTTP on any port, for the loopback test server. Test builds only.
    #[cfg(test)]
    fn lenient(hosts: &[&str]) -> Self {
        Self {
            allowed_hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
            strict: false,
        }
    }

    fn check(&self, uri: &Uri) -> Result<(), UpdateError> {
        let scheme = uri.scheme_str().unwrap_or("");
        let scheme_ok = if self.strict {
            scheme == "https"
        } else {
            scheme == "https" || scheme == "http"
        };
        if !scheme_ok {
            return Err(UpdateError::InsecureUrl(display_origin(uri)));
        }
        let Some(authority) = uri.authority() else {
            return Err(UpdateError::HostNotAllowed(String::new()));
        };
        if authority.as_str().contains('@') {
            // user:password@host — never legitimate for the updater.
            return Err(UpdateError::HostNotAllowed(
                "(URL with credentials)".to_owned(),
            ));
        }
        let host = authority.host().to_ascii_lowercase();
        if !self.allowed_hosts.contains(&host) {
            return Err(UpdateError::HostNotAllowed(host));
        }
        if self.strict && !matches!(authority.port_u16(), None | Some(443)) {
            return Err(UpdateError::HostNotAllowed(format!(
                "{host} (non-default port)"
            )));
        }
        Ok(())
    }
}

/// `scheme://host` of a URI, for error messages. Never the path or query: a
/// signed asset URL carries a token there.
fn display_origin(uri: &Uri) -> String {
    format!(
        "{}://{}",
        uri.scheme_str().unwrap_or("?"),
        uri.host().unwrap_or("?")
    )
}

/// Marker error carried inside an `io::Error` when a body exceeds its cap.
#[derive(Debug)]
struct BodyTooLarge {
    limit: u64,
}

impl fmt::Display for BodyTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "response body exceeds {} bytes", self.limit)
    }
}

impl std::error::Error for BodyTooLarge {}

/// Turns an error from reading a response body into an [`UpdateError`].
pub fn map_read_error(error: &io::Error) -> UpdateError {
    if let Some(too_large) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<BodyTooLarge>())
    {
        return UpdateError::TooLarge {
            limit: too_large.limit,
        };
    }
    UpdateError::Network(error.to_string())
}

/// Reader that fails once more than `limit` bytes have been read.
struct LimitedReader<R> {
    inner: R,
    remaining: u64,
    limit: u64,
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        let read_u64 = read as u64;
        if read_u64 > self.remaining {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                BodyTooLarge { limit: self.limit },
            ));
        }
        self.remaining -= read_u64;
        Ok(read)
    }
}

/// An open response body, capped at the resource's size limit.
pub struct Fetched {
    content_length: Option<u64>,
    reader: Box<dyn Read + Send>,
}

impl Fetched {
    /// `Content-Length`, when the server sent one.
    pub fn content_length(&self) -> Option<u64> {
        self.content_length
    }
}

impl Read for Fetched {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

/// Blocking HTTP client for the updater. Callers run it under `spawn_blocking`.
pub struct UpdateClient {
    policy: Policy,
    api_base: String,
    small_agent: Agent,
    package_agent: Agent,
}

impl UpdateClient {
    /// The production client: GitHub's allowlist, HTTPS only.
    pub fn new() -> Self {
        Self::production_with_proxy(system_proxy())
    }

    fn production_with_proxy(proxy: Option<ureq::Proxy>) -> Self {
        let policy = Policy::production();
        Self {
            small_agent: build_agent(&policy, SMALL_BODY_TOTAL_TIMEOUT, proxy.clone()),
            package_agent: build_agent(&policy, PACKAGE_BODY_TOTAL_TIMEOUT, proxy),
            policy,
            api_base: API_BASE.to_owned(),
        }
    }

    /// Production policy (strict allowlist, HTTPS) routed through an explicit
    /// proxy, to observe what the updater asks a proxy for. Test builds only.
    #[cfg(test)]
    pub(crate) fn with_proxy_for_test(proxy_url: &str) -> Self {
        Self::production_with_proxy(Some(ureq::Proxy::new(proxy_url).expect("valid proxy URL")))
    }

    /// Client against a loopback test server. Test builds only: no release
    /// build contains this constructor or a lenient [`Policy`].
    #[cfg(test)]
    pub(crate) fn for_test(api_base: &str, hosts: &[&str]) -> Self {
        let policy = Policy::lenient(hosts);
        Self {
            small_agent: build_agent(&policy, Duration::from_secs(10), None),
            package_agent: build_agent(&policy, Duration::from_secs(10), None),
            policy,
            api_base: api_base.trim_end_matches('/').to_owned(),
        }
    }

    /// `GET <api>/repos/<owner>/<repo>/releases/latest`.
    pub fn latest_release_url(&self) -> String {
        format!("{}/repos/{REPOSITORY}/releases/latest", self.api_base)
    }

    /// Fetches a whole (small) body, capped at the resource's limit.
    pub fn fetch_bytes(&self, url: &str, resource: Resource) -> Result<Vec<u8>, UpdateError> {
        self.fetch_bytes_limited(url, resource, resource.max_bytes())
    }

    /// Opens a streaming body, capped at the resource's limit.
    pub fn open(&self, url: &str, resource: Resource) -> Result<Fetched, UpdateError> {
        self.open_limited(url, resource, resource.max_bytes())
    }

    fn fetch_bytes_limited(
        &self,
        url: &str,
        resource: Resource,
        max_bytes: u64,
    ) -> Result<Vec<u8>, UpdateError> {
        let mut fetched = self.open_limited(url, resource, max_bytes)?;
        let mut bytes = Vec::new();
        fetched
            .read_to_end(&mut bytes)
            .map_err(|e| map_read_error(&e))?;
        Ok(bytes)
    }

    fn open_limited(
        &self,
        url: &str,
        resource: Resource,
        max_bytes: u64,
    ) -> Result<Fetched, UpdateError> {
        let agent = match resource {
            Resource::Package => &self.package_agent,
            Resource::ApiJson | Resource::Checksum => &self.small_agent,
        };
        let mut current: Uri = url
            .parse()
            .map_err(|_| UpdateError::InvalidResponse("malformed URL".to_owned()))?;

        for redirects_followed in 0..=MAX_REDIRECTS {
            // Checked before every connection, including the first.
            self.policy.check(&current)?;
            tracing::debug!(
                host = current.host().unwrap_or(""),
                redirects_followed,
                "update request"
            );

            let response = agent
                .get(current.to_string())
                .header("Accept", resource.accept())
                .call()
                .map_err(|e| UpdateError::Network(e.to_string()))?;
            let status = response.status().as_u16();

            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if redirects_followed == MAX_REDIRECTS {
                    return Err(UpdateError::Network("too many redirects".to_owned()));
                }
                current = redirect_target(&current, response.headers())?;
                continue;
            }
            if is_rate_limited(status, response.headers()) {
                return Err(UpdateError::RateLimited {
                    reset_at_unix: rate_limit_reset(response.headers()),
                });
            }
            if status != 200 {
                return Err(UpdateError::UnexpectedStatus(status));
            }

            let body = response.into_body();
            let content_length = body.content_length();
            if content_length.is_some_and(|len| len > max_bytes) {
                return Err(UpdateError::TooLarge { limit: max_bytes });
            }
            return Ok(Fetched {
                content_length,
                reader: Box::new(LimitedReader {
                    inner: body.into_reader(),
                    remaining: max_bytes,
                    limit: max_bytes,
                }),
            });
        }
        // The loop returns on every path; the last iteration errors on a redirect.
        Err(UpdateError::Network("too many redirects".to_owned()))
    }
}

impl Default for UpdateClient {
    fn default() -> Self {
        Self::new()
    }
}

/// The proxy taken from the environment (`ALL_PROXY`, `HTTPS_PROXY`,
/// `HTTP_PROXY`, lower-case variants too, minus `NO_PROXY` matches).
///
/// This is deliberate, not an accident of `ureq`'s defaults: ADR-015 records
/// that the updater follows the system proxy like any HTTPS client, because
/// behind a corporate proxy it would otherwise never work. It is read once,
/// when the client is built, which happens only inside the *Check for
/// Updates…* handler. The proxy host is the one destination outside the
/// allowlist, and it only ever receives a `CONNECT <allowlisted host>:443`.
fn system_proxy() -> Option<ureq::Proxy> {
    ureq::Proxy::try_from_env()
}

/// Builds an agent for the given policy. `proxy` is `None` for the loopback
/// test server (which must never go through a proxy) and for a machine
/// without proxy variables.
fn build_agent(policy: &Policy, body_total: Duration, proxy: Option<ureq::Proxy>) -> Agent {
    // TLS is spelled out so that it cannot silently change with a dependency
    // upgrade: rustls, Mozilla's roots (no platform verifier, so a proxy that
    // re-signs traffic is not trusted), certificate verification on. The
    // server name checked is the URL's host, also when tunnelled via CONNECT.
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::WebPki)
        .disable_verification(false)
        .build();
    Agent::config_builder()
        .https_only(policy.strict)
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(proxy)
        .tls_config(tls)
        .user_agent(format!("SignalLock/{}", env!("CARGO_PKG_VERSION")))
        .timeout_resolve(Some(CONNECT_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .timeout_per_read(Some(READ_TIMEOUT))
        .timeout_recv_body(Some(body_total))
        .build()
        .into()
}

/// Resolves a `Location` header against the URI that produced it.
fn redirect_target(from: &Uri, headers: &HeaderMap) -> Result<Uri, UpdateError> {
    let location = headers
        .get("location")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| UpdateError::Network("redirect without Location".to_owned()))?;

    let absolute = if location.starts_with('/') && !location.starts_with("//") {
        let scheme = from.scheme_str().unwrap_or("https");
        let authority = from.authority().map(|a| a.as_str()).unwrap_or("");
        format!("{scheme}://{authority}{location}")
    } else {
        location.to_owned()
    };
    let target: Uri = absolute
        .parse()
        .map_err(|_| UpdateError::Network("redirect to a malformed URL".to_owned()))?;
    if target.scheme().is_none() || target.authority().is_none() {
        return Err(UpdateError::Network(
            "redirect to a relative URL".to_owned(),
        ));
    }
    Ok(target)
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// `429`, or `403` together with an exhausted quota or a `Retry-After`.
fn is_rate_limited(status: u16, headers: &HeaderMap) -> bool {
    match status {
        429 => true,
        403 => {
            header_str(headers, "x-ratelimit-remaining").is_some_and(|v| v.trim() == "0")
                || headers.contains_key("retry-after")
        }
        _ => false,
    }
}

/// When the quota resets, as Unix seconds (`x-ratelimit-reset`, else
/// `retry-after` relative to now).
fn rate_limit_reset(headers: &HeaderMap) -> Option<u64> {
    if let Some(reset) =
        header_str(headers, "x-ratelimit-reset").and_then(|v| v.trim().parse().ok())
    {
        return Some(reset);
    }
    let retry_after: u64 = header_str(headers, "retry-after")?.trim().parse().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(now.saturating_add(retry_after))
}

/// Loopback HTTP server for tests. One request per connection, no keep-alive.
#[cfg(test)]
pub(crate) mod test_server {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    #[derive(Debug, Clone)]
    pub struct Reply {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
        /// Omit `Content-Length` and close the connection to end the body.
        pub unsized_body: bool,
    }

    impl Reply {
        pub fn ok(body: impl Into<Vec<u8>>) -> Self {
            Self {
                status: 200,
                headers: Vec::new(),
                body: body.into(),
                unsized_body: false,
            }
        }

        pub fn status(status: u16) -> Self {
            Self {
                status,
                ..Self::ok(Vec::new())
            }
        }

        pub fn redirect(location: &str) -> Self {
            Self::status(302).header("Location", location)
        }

        pub fn header(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.to_owned(), value.to_owned()));
            self
        }

        pub fn unsized_body(mut self) -> Self {
            self.unsized_body = true;
            self
        }
    }

    #[derive(Debug, Clone)]
    pub struct Seen {
        pub method: String,
        /// Request target; for `CONNECT` the `host:port`.
        pub path: String,
        /// Header names lowercased.
        pub headers: HashMap<String, String>,
    }

    pub struct TestServer {
        pub base: String,
        pub host: String,
        routes: Arc<Mutex<HashMap<String, Reply>>>,
        seen: Arc<Mutex<Vec<Seen>>>,
        stop: Arc<AtomicBool>,
        addr: std::net::SocketAddr,
        handle: Option<JoinHandle<()>>,
    }

    impl TestServer {
        pub fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let addr = listener.local_addr().expect("local addr");
            let routes: Arc<Mutex<HashMap<String, Reply>>> = Arc::default();
            let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
            let stop = Arc::new(AtomicBool::new(false));

            let handle = {
                let routes = Arc::clone(&routes);
                let seen = Arc::clone(&seen);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        if let Ok(stream) = stream {
                            serve(stream, &routes, &seen);
                        }
                    }
                })
            };
            Self {
                base: format!("http://{addr}"),
                host: "127.0.0.1".to_owned(),
                routes,
                seen,
                stop,
                addr,
                handle: Some(handle),
            }
        }

        pub fn route(&self, path: &str, reply: Reply) {
            self.routes
                .lock()
                .expect("routes lock")
                .insert(path.to_owned(), reply);
        }

        pub fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }

        pub fn seen(&self) -> Vec<Seen> {
            self.seen.lock().expect("seen lock").clone()
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // Wake the blocking accept().
            let _ = TcpStream::connect(self.addr);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn serve(stream: TcpStream, routes: &Mutex<HashMap<String, Reply>>, seen: &Mutex<Vec<Seen>>) {
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let method = request_line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_owned();
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_owned();
        let mut headers = HashMap::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
            }
        }
        seen.lock().expect("seen lock").push(Seen {
            method,
            path: path.clone(),
            headers,
        });

        let reply = routes
            .lock()
            .expect("routes lock")
            .get(&path)
            .cloned()
            .unwrap_or_else(|| Reply::status(404));

        let mut out = format!("HTTP/1.1 {} Test\r\nConnection: close\r\n", reply.status);
        for (name, value) in &reply.headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        if !reply.unsized_body {
            out.push_str(&format!("Content-Length: {}\r\n", reply.body.len()));
        }
        out.push_str("\r\n");
        let mut stream = &stream;
        let _ = stream.write_all(out.as_bytes());
        let _ = stream.write_all(&reply.body);
        let _ = stream.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{Reply, TestServer};
    use super::*;

    fn uri(s: &str) -> Uri {
        s.parse().expect("valid test URI")
    }

    fn production_check(url: &str) -> Result<(), UpdateError> {
        Policy::production().check(&uri(url))
    }

    // --- allowlist -------------------------------------------------------

    #[test]
    fn allowlist_accepts_every_listed_host_over_https() {
        for host in ALLOWED_HOSTS {
            assert_eq!(production_check(&format!("https://{host}/x?y=1")), Ok(()));
        }
        assert_eq!(production_check("https://API.GitHub.com/x"), Ok(()));
        assert_eq!(production_check("https://github.com:443/x"), Ok(()));
    }

    #[test]
    fn allowlist_rejects_foreign_and_lookalike_hosts() {
        for url in [
            "https://example.com/x",
            "https://api.github.com.evil.example/x",
            "https://evilgithub.com/x",
            "https://raw.githubusercontent.com/x",
            "https://uploads.github.com/x",
            "https://sub.github.com/x",
        ] {
            assert!(
                matches!(production_check(url), Err(UpdateError::HostNotAllowed(_))),
                "{url} must be rejected"
            );
        }
    }

    #[test]
    fn allowlist_rejects_http_other_ports_and_credentials() {
        assert!(matches!(
            production_check("http://api.github.com/x"),
            Err(UpdateError::InsecureUrl(_))
        ));
        assert!(matches!(
            production_check("ftp://github.com/x"),
            Err(UpdateError::InsecureUrl(_))
        ));
        assert!(matches!(
            production_check("https://github.com:8443/x"),
            Err(UpdateError::HostNotAllowed(_))
        ));
        assert!(matches!(
            production_check("https://user:pw@github.com/x"),
            Err(UpdateError::HostNotAllowed(_))
        ));
        assert!(matches!(
            production_check("https://github.com@evil.example/x"),
            Err(UpdateError::HostNotAllowed(_))
        ));
    }

    #[test]
    fn insecure_url_error_never_carries_path_or_query() {
        let Err(UpdateError::InsecureUrl(origin)) =
            production_check("http://github.com/secret/path?sig=abc")
        else {
            panic!("expected InsecureUrl");
        };
        assert_eq!(origin, "http://github.com");
    }

    #[test]
    fn production_client_targets_the_github_api() {
        assert_eq!(
            UpdateClient::new().latest_release_url(),
            "https://api.github.com/repos/muhammetsafak/signallock/releases/latest"
        );
    }

    // --- fetching and redirects -----------------------------------------

    fn client_for(server: &TestServer) -> UpdateClient {
        UpdateClient::for_test(&server.base, &[&server.host])
    }

    #[test]
    fn fetch_sends_only_user_agent_and_accept() {
        let server = TestServer::start();
        server.route("/data", Reply::ok(b"hello".to_vec()));
        let bytes = client_for(&server)
            .fetch_bytes(&server.url("/data"), Resource::ApiJson)
            .expect("fetch");
        assert_eq!(bytes, b"hello");

        let seen = server.seen();
        assert_eq!(seen.len(), 1);
        let headers = &seen[0].headers;
        assert_eq!(
            headers.get("user-agent").map(String::as_str),
            Some(format!("SignalLock/{}", env!("CARGO_PKG_VERSION")).as_str())
        );
        assert_eq!(
            headers.get("accept").map(String::as_str),
            Some("application/vnd.github+json")
        );
        for forbidden in ["cookie", "authorization", "x-github-api-version"] {
            assert!(!headers.contains_key(forbidden), "{forbidden} was sent");
        }
    }

    #[test]
    fn asset_requests_ask_for_octet_stream() {
        let server = TestServer::start();
        server.route("/a", Reply::ok(b"x".to_vec()));
        client_for(&server)
            .fetch_bytes(&server.url("/a"), Resource::Checksum)
            .expect("fetch");
        assert_eq!(
            server.seen()[0].headers.get("accept").map(String::as_str),
            Some("application/octet-stream")
        );
    }

    #[test]
    fn redirects_are_followed_by_hand_and_checked() {
        let server = TestServer::start();
        server.route("/start", Reply::redirect("/middle"));
        server.route("/middle", Reply::redirect(&server.url("/end")));
        server.route("/end", Reply::ok(b"done".to_vec()));
        let bytes = client_for(&server)
            .fetch_bytes(&server.url("/start"), Resource::Package)
            .expect("fetch through two redirects");
        assert_eq!(bytes, b"done");
        let paths: Vec<_> = server.seen().into_iter().map(|s| s.path).collect();
        assert_eq!(paths, ["/start", "/middle", "/end"]);
    }

    #[test]
    fn five_redirects_are_followed_the_sixth_is_refused() {
        let server = TestServer::start();
        for hop in 0..6 {
            server.route(
                &format!("/r{hop}"),
                Reply::redirect(&format!("/r{}", hop + 1)),
            );
        }
        server.route("/r5", Reply::ok(b"five".to_vec()));
        let client = client_for(&server);
        assert_eq!(
            client
                .fetch_bytes(&server.url("/r0"), Resource::ApiJson)
                .expect("five redirects"),
            b"five"
        );

        // r5 now redirects too: that is the sixth hop.
        server.route("/r5", Reply::redirect("/r6"));
        server.route("/r6", Reply::ok(b"six".to_vec()));
        let error = client
            .fetch_bytes(&server.url("/r0"), Resource::ApiJson)
            .expect_err("six redirects");
        assert_eq!(error, UpdateError::Network("too many redirects".to_owned()));
    }

    #[test]
    fn redirect_outside_the_allowlist_is_refused_before_connecting() {
        let server = TestServer::start();
        server.route("/start", Reply::redirect("http://evil.example/payload"));
        let error = client_for(&server)
            .fetch_bytes(&server.url("/start"), Resource::Package)
            .expect_err("redirect to a foreign host");
        assert_eq!(
            error,
            UpdateError::HostNotAllowed("evil.example".to_owned())
        );
    }

    #[test]
    fn redirect_to_an_https_downgrade_is_refused_by_a_strict_policy() {
        // The strict policy is what production uses: a redirect from an
        // allowed HTTPS host to plain HTTP must not be followed.
        let from = uri("https://github.com/a");
        let mut headers = HeaderMap::new();
        headers.insert("location", "http://github.com/b".parse().expect("header"));
        let target = redirect_target(&from, &headers).expect("parses");
        assert!(matches!(
            Policy::production().check(&target),
            Err(UpdateError::InsecureUrl(_))
        ));
    }

    #[test]
    fn redirect_without_location_or_with_scheme_relative_location_fails() {
        let from = uri("https://github.com/a");
        let empty = HeaderMap::new();
        assert!(redirect_target(&from, &empty).is_err());
        let mut headers = HeaderMap::new();
        headers.insert("location", "//evil.example/x".parse().expect("header"));
        assert!(redirect_target(&from, &headers).is_err());
        let mut headers = HeaderMap::new();
        headers.insert("location", "relative/path".parse().expect("header"));
        assert!(redirect_target(&from, &headers).is_err());
    }

    #[test]
    fn relative_redirect_keeps_scheme_and_authority() {
        let from = uri("https://github.com/a/b");
        let mut headers = HeaderMap::new();
        headers.insert("location", "/c?d=1".parse().expect("header"));
        assert_eq!(
            redirect_target(&from, &headers)
                .expect("resolves")
                .to_string(),
            "https://github.com/c?d=1"
        );
    }

    // --- status handling --------------------------------------------------

    #[test]
    fn rate_limit_403_with_exhausted_quota_reports_the_reset_time() {
        let server = TestServer::start();
        server.route(
            "/api",
            Reply::status(403)
                .header("x-ratelimit-remaining", "0")
                .header("x-ratelimit-reset", "1893456000"),
        );
        let error = client_for(&server)
            .fetch_bytes(&server.url("/api"), Resource::ApiJson)
            .expect_err("rate limited");
        assert_eq!(
            error,
            UpdateError::RateLimited {
                reset_at_unix: Some(1_893_456_000)
            }
        );
    }

    #[test]
    fn rate_limit_429_is_always_rate_limited() {
        let server = TestServer::start();
        server.route("/api", Reply::status(429));
        let error = client_for(&server)
            .fetch_bytes(&server.url("/api"), Resource::ApiJson)
            .expect_err("429");
        assert_eq!(
            error,
            UpdateError::RateLimited {
                reset_at_unix: None
            }
        );
    }

    #[test]
    fn retry_after_is_turned_into_a_reset_time() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "120".parse().expect("header"));
        let reset = rate_limit_reset(&headers).expect("reset");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        assert!((now + 118..=now + 122).contains(&reset));
    }

    #[test]
    fn plain_403_and_404_are_unexpected_status_not_rate_limit() {
        let server = TestServer::start();
        server.route(
            "/forbidden",
            Reply::status(403).header("x-ratelimit-remaining", "7"),
        );
        let client = client_for(&server);
        assert_eq!(
            client
                .fetch_bytes(&server.url("/forbidden"), Resource::ApiJson)
                .expect_err("403"),
            UpdateError::UnexpectedStatus(403)
        );
        assert_eq!(
            client
                .fetch_bytes(&server.url("/missing"), Resource::ApiJson)
                .expect_err("404"),
            UpdateError::UnexpectedStatus(404)
        );
    }

    #[test]
    fn connection_failure_is_a_network_error() {
        // Port 1 on loopback has no listener, and unlike "bind a port, then
        // drop it" no parallel test's server can ever be handed that port.
        let client = UpdateClient::for_test("http://127.0.0.1:1", &["127.0.0.1"]);
        let error = client
            .fetch_bytes("http://127.0.0.1:1/x", Resource::ApiJson)
            .expect_err("nothing listens on port 1");
        assert!(matches!(error, UpdateError::Network(_)), "{error:?}");
    }

    // --- proxy ------------------------------------------------------------

    /// The test server doubles as a proxy: it records the request line a
    /// proxy would receive and answers `404`, which makes the tunnel fail.
    #[test]
    fn through_a_proxy_the_connect_target_is_the_allowlisted_host() {
        let proxy = TestServer::start();
        let client = UpdateClient::with_proxy_for_test(&proxy.base);
        let error = client
            .fetch_bytes(
                "https://api.github.com/repos/muhammetsafak/signallock/releases/latest",
                Resource::ApiJson,
            )
            .expect_err("the fake proxy refuses the tunnel");
        assert!(matches!(error, UpdateError::Network(_)), "{error:?}");

        let seen = proxy.seen();
        assert_eq!(seen.len(), 1, "exactly one request reaches the proxy");
        assert_eq!(seen[0].method, "CONNECT");
        assert_eq!(seen[0].path, "api.github.com:443");
        // Nothing but the tunnel request: no URL path, no header of ours leaks
        // into the cleartext part of the exchange.
        assert!(!seen[0].headers.contains_key("authorization"));
        assert!(!seen[0].headers.contains_key("cookie"));
    }

    #[test]
    fn through_a_proxy_a_foreign_host_never_reaches_the_proxy() {
        let proxy = TestServer::start();
        let client = UpdateClient::with_proxy_for_test(&proxy.base);
        for url in [
            "https://evil.example/payload",
            "https://api.github.com.evil.example/x",
            "http://api.github.com/x",
        ] {
            let error = client
                .fetch_bytes(url, Resource::Package)
                .expect_err("refused before any connection");
            assert!(
                matches!(
                    error,
                    UpdateError::HostNotAllowed(_) | UpdateError::InsecureUrl(_)
                ),
                "{url}: {error:?}"
            );
        }
        assert!(
            proxy.seen().is_empty(),
            "a refused URL must not even produce a CONNECT"
        );
    }

    #[test]
    fn tls_is_rustls_with_mozilla_roots_and_verification_on() {
        // Both the production and the loopback agents keep verification on;
        // only the URL policy differs between them.
        for client in [
            UpdateClient::new(),
            UpdateClient::for_test("http://127.0.0.1:1", &["127.0.0.1"]),
        ] {
            for agent in [&client.small_agent, &client.package_agent] {
                let tls = agent.config().tls_config();
                assert!(!tls.disable_verification());
                assert!(matches!(tls.provider(), ureq::tls::TlsProvider::Rustls));
                assert!(matches!(tls.root_certs(), ureq::tls::RootCerts::WebPki));
            }
        }
    }

    // --- size caps --------------------------------------------------------

    #[test]
    fn declared_oversize_body_is_refused_before_reading() {
        let server = TestServer::start();
        server.route("/big", Reply::ok(vec![b'a'; 2000]));
        let error = client_for(&server)
            .fetch_bytes(&server.url("/big"), Resource::Checksum)
            .expect_err("over the 1 KiB checksum cap");
        assert_eq!(
            error,
            UpdateError::TooLarge {
                limit: CHECKSUM_MAX_BYTES
            }
        );
    }

    #[test]
    fn undeclared_oversize_body_is_cut_off_while_reading() {
        let server = TestServer::start();
        server.route("/big", Reply::ok(vec![b'a'; 5000]).unsized_body());
        let error = client_for(&server)
            .fetch_bytes(&server.url("/big"), Resource::Checksum)
            .expect_err("over the cap without Content-Length");
        assert_eq!(
            error,
            UpdateError::TooLarge {
                limit: CHECKSUM_MAX_BYTES
            }
        );
    }

    #[test]
    fn body_exactly_at_the_cap_is_accepted_and_one_byte_more_is_not() {
        let server = TestServer::start();
        server.route("/exact", Reply::ok(vec![b'a'; 100]).unsized_body());
        server.route("/over", Reply::ok(vec![b'a'; 101]).unsized_body());
        let client = client_for(&server);
        assert_eq!(
            client
                .fetch_bytes_limited(&server.url("/exact"), Resource::Package, 100)
                .expect("exactly at the cap")
                .len(),
            100
        );
        assert_eq!(
            client
                .fetch_bytes_limited(&server.url("/over"), Resource::Package, 100)
                .expect_err("one byte over"),
            UpdateError::TooLarge { limit: 100 }
        );
    }

    #[test]
    fn limited_reader_errors_with_the_marker_type() {
        let mut reader = LimitedReader {
            inner: io::Cursor::new(vec![0_u8; 10]),
            remaining: 4,
            limit: 4,
        };
        let mut sink = Vec::new();
        let error = reader.read_to_end(&mut sink).expect_err("over the limit");
        assert_eq!(map_read_error(&error), UpdateError::TooLarge { limit: 4 });
        assert!(matches!(
            map_read_error(&io::Error::other("boom")),
            UpdateError::Network(_)
        ));
    }

    #[test]
    fn package_caps_match_the_adr() {
        assert_eq!(Resource::ApiJson.max_bytes(), 1024 * 1024);
        assert_eq!(Resource::Package.max_bytes(), 200 * 1024 * 1024);
        assert_eq!(Resource::Checksum.max_bytes(), 1024);
    }
}
