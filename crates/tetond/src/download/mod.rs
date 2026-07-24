//! The model-download HTTP client — the second of the daemon's two trust
//! contexts (REQ-547 D-2).
//!
//! `teton_inference` owns the *orchestration* of a model download (resume,
//! verify, discard-and-refetch) behind a byte-range transport trait
//! ([`RangeFetcher`]), and deliberately takes on no HTTP dependency. Until this
//! module existed that trait had only `#[cfg(test)]` implementors, so no real
//! download could happen. [`HttpRangeFetcher`] is the production implementation.
//!
//! ## Why this is not the egress client (D-2)
//!
//! | Client | Credentials | Redirects | Purpose |
//! |---|---|---|---|
//! | [`egress::HttpTransport`](crate::egress::HttpTransport) | endpoint-bound auth headers | **refused** | provider + MCP traffic |
//! | [`HttpRangeFetcher`] (here) | **none, ever** | **followed** | GGUF artifacts from the model host → its CDN |
//!
//! A model fetch needs to follow redirects — a HuggingFace `/resolve/` URL
//! answers `302` with a CDN `Location` on a different host — and the egress
//! client must never follow one, because `reqwest` strips `Authorization` across
//! a host change but **not** a custom credential header like `x-api-key`, so a
//! followed redirect could carry a provider secret to an attacker-influenced
//! host. Rather than weaken that policy for the benefit of a download, the
//! download gets its own client that has no credential to leak: no default
//! headers, no injected auth, and a refusal to fetch a URL that embeds
//! credentials in its userinfo (which `reqwest` would turn into an
//! `Authorization: Basic` header — see `validate_url`).
//!
//! Keeping the two clients apart is the point: it means a future "just allow
//! redirects" change cannot re-open the credential-forwarding hole the egress
//! policy closed. The `two_client_posture` test module asserts **both** postures
//! together, so relaxing either one fails the same test file (BR-14, AC-11).
//! Both clients still live in `tetond`, preserving the `deny_http_client`
//! invariant that this crate holds the workspace's only HTTP client (BR-1).
//!
//! ## Blocking trait, async client
//!
//! [`RangeFetcher::fetch`] is synchronous (the orchestration around it is
//! straight-line file I/O), while `reqwest`'s client is async. Each fetch
//! therefore runs its request on a short-lived worker thread with its own
//! current-thread runtime, streaming chunks back over a bounded channel that the
//! calling thread drains into the sink. That costs a thread per resumed segment
//! — a handful per download — and buys a fetcher that is safe to call from *any*
//! context: `reqwest`'s blocking client panics when called from inside an async
//! runtime, and `Runtime::block_on` panics when called from inside another
//! runtime's worker. This one does neither.
//!
//! ## Error surface
//!
//! [`FetchError`] classifies failures precisely — a rate limit (`429`) and an
//! availability failure (`5xx`) stay distinct from each other and from a corrupt
//! download (`DownloadError::Checksum`, raised by the library's verifier), per
//! AC-12. The trait can only return [`DownloadError`], whose transport variant
//! carries a string, so the precise classification is *also* retained on the
//! fetcher and readable through [`HttpRangeFetcher::last_error`] after a failed
//! download — that is what lets the install pipeline (TASK-005) tell a user
//! "the model host is rate-limiting; try again shortly" instead of a generic
//! transport failure.

pub mod backoff;

use std::io;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::{HeaderMap, CONTENT_RANGE, RANGE, RETRY_AFTER};

use teton_inference::{DownloadError, RangeFetcher};

pub use backoff::{retry_class, RetryClass, RetryPolicy};

/// Redirect hops allowed before the fetch fails. A model host → CDN handoff is
/// one hop; the allowance matches `reqwest`'s own default and exists only to
/// bound a misconfigured mirror's redirect loop.
const MAX_REDIRECTS: usize = 10;

/// Connect timeout. Deliberately *not* a whole-request timeout: the request
/// bodies here are multi-gigabyte and a total deadline would abort healthy slow
/// downloads.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-read inactivity timeout. Bounds a silently wedged connection without
/// bounding the transfer; the orchestrator resumes from the bytes already
/// written.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Body chunks buffered between the network worker and the calling thread.
/// Small on purpose: the sink writes to disk, so the buffer only needs to cover
/// scheduling jitter, and a deep queue would just hold megabytes hostage.
const CHANNEL_DEPTH: usize = 8;

/// A classified model-download failure.
///
/// Every variant is content-free by construction — a status code, a failure
/// class, an attempt count. No URL, no header, no body ever reaches a log line
/// through this type (conventions: nothing content-bearing in logs).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// The host is rate-limiting the download (`429`) and kept doing so for the
    /// whole retry ladder (BR-16).
    #[error(
        "the model host rate-limited the download (HTTP {status}) after {attempts} attempt(s)"
    )]
    RateLimited {
        /// The status that triggered the ladder.
        status: u16,
        /// Attempts made, including the first.
        attempts: u32,
    },
    /// The host or its CDN is unavailable (`5xx`) for the whole retry ladder.
    #[error("the model host was unavailable (HTTP {status}) after {attempts} attempt(s)")]
    Unavailable {
        /// The status that triggered the ladder.
        status: u16,
        /// Attempts made, including the first.
        attempts: u32,
    },
    /// A non-retryable HTTP status (`404`, `403`, `416`, an unfollowed 3xx …).
    #[error("the model host returned HTTP {status}")]
    Http {
        /// The status returned.
        status: u16,
    },
    /// A network-level failure, reduced to a failure class.
    #[error("network {class} failure while fetching the model")]
    Network {
        /// Failure class: `connect`, `timeout`, `body`, `io`, or `aborted`.
        class: &'static str,
    },
    /// The redirect chain exceeded `MAX_REDIRECTS` — a loop or a broken mirror.
    #[error("the model host redirected more than {MAX_REDIRECTS} times")]
    TooManyRedirects,
    /// The URL is not a fetchable `http(s)` URL.
    #[error("the model URL is not a fetchable http(s) URL")]
    InvalidUrl,
    /// The URL embeds userinfo credentials. Refused rather than stripped: this
    /// client's contract is that it carries no credential, and `reqwest` would
    /// turn userinfo into an `Authorization: Basic` header (D-2).
    #[error("refusing to fetch a model URL that embeds credentials")]
    Credentialed,
    /// The host answered a range request with a range we cannot resume from.
    #[error("the model host answered the range request with an unusable range")]
    BadRange,
    /// The HTTP client could not be initialized.
    #[error("failed to initialize the model-download HTTP client")]
    ClientInit,
}

impl FetchError {
    /// Whether the failure is worth trying again later.
    ///
    /// Transient failures map onto [`DownloadError::Transport`], which the
    /// library's orchestrator treats as a resumable interruption: bytes already
    /// written stay written and the next attempt resumes from that offset.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            FetchError::RateLimited { .. }
                | FetchError::Unavailable { .. }
                | FetchError::Network { .. }
        )
    }

    /// Whether the host rate-limited us — distinct from an availability failure
    /// and from a corrupt download (AC-12).
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, FetchError::RateLimited { .. })
    }

    /// Project onto the library's error type at the [`RangeFetcher`] seam.
    ///
    /// The seam's contract gives one variant per *orchestration* behaviour, not
    /// one per cause: [`DownloadError::Transport`] means "resume from the
    /// current offset", so only transient failures may take it. A permanent
    /// failure must stop the download immediately, which the orchestrator only
    /// does for a non-`Transport` error — hence the I/O variant, whose message
    /// still carries the precise cause. The typed cause is kept verbatim on
    /// [`HttpRangeFetcher::last_error`] for the caller that wants to act on it.
    #[must_use]
    fn to_download_error(&self) -> DownloadError {
        if self.is_transient() {
            DownloadError::Transport(self.to_string())
        } else {
            DownloadError::Io(io::Error::other(self.to_string()))
        }
    }
}

/// A failure inside one [`HttpRangeFetcher::fetch`] call.
enum FetchFailure {
    /// Ours: a classified transport failure.
    Client(FetchError),
    /// The caller's: the sink rejected a chunk (a full disk, an oversized
    /// stream). Passed through untouched — reclassifying a caller's error would
    /// turn "the artifact is oversized" into "the network hiccuped".
    Sink(DownloadError),
}

/// Messages from the network worker to the calling thread.
enum Message {
    /// The resource's total length: from the response headers when declared,
    /// otherwise computed once the body ends.
    Length(u64),
    /// A run of body bytes at the caller's current offset.
    Chunk(Vec<u8>),
    /// The attempt failed; no further messages follow.
    Failed(FetchError),
}

/// The production [`RangeFetcher`]: credential-free, redirect-following, with a
/// 429/503 retry ladder (D-2, BR-13, BR-14, BR-16).
#[derive(Debug)]
pub struct HttpRangeFetcher {
    client: reqwest::Client,
    policy: RetryPolicy,
    last_error: Mutex<Option<FetchError>>,
}

impl HttpRangeFetcher {
    /// Build a fetcher with the default retry policy.
    ///
    /// # Errors
    /// Returns [`FetchError::ClientInit`] if the HTTP client cannot be built
    /// (a missing TLS backend, an unreadable resolver configuration).
    pub fn new() -> Result<Self, FetchError> {
        Self::with_policy(RetryPolicy::default())
    }

    /// Build a fetcher with an explicit retry policy.
    ///
    /// # Errors
    /// Returns [`FetchError::ClientInit`] if the HTTP client cannot be built.
    pub fn with_policy(policy: RetryPolicy) -> Result<Self, FetchError> {
        let client = reqwest::Client::builder()
            // D-2: no default headers. Nothing this client sends is
            // configurable from the outside, so no credential can be attached
            // to it by construction — the counterpart to the egress client,
            // which attaches an endpoint-bound credential on purpose.
            .default_headers(HeaderMap::new())
            // D-2: follow redirects. A `/resolve/` URL answers 302 with a CDN
            // `Location` on another host. This is safe *here and only here*
            // because there is no credential to carry across the hop; the
            // egress client keeps `Policy::none()` for exactly that reason.
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|_| FetchError::ClientInit)?;
        Ok(Self {
            client,
            policy,
            last_error: Mutex::new(None),
        })
    }

    /// The retry policy in force.
    #[must_use]
    pub fn policy(&self) -> RetryPolicy {
        self.policy
    }

    /// The classified cause of the most recent failed fetch, if the last fetch
    /// failed. Cleared by a successful fetch.
    ///
    /// This is how a caller distinguishes "the host is rate-limiting us" from
    /// "the host is down" from "the artifact was corrupt" (AC-12) even though
    /// the [`RangeFetcher`] seam can only hand back a [`DownloadError`].
    #[must_use]
    pub fn last_error(&self) -> Option<FetchError> {
        self.last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_last_error(&self, error: Option<FetchError>) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = error;
    }

    /// One fetch: spawn the network worker, drain its chunks into `sink`.
    fn fetch_inner(
        &self,
        url: &str,
        offset: u64,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
    ) -> Result<u64, FetchFailure> {
        let target = validate_url(url).map_err(FetchFailure::Client)?;
        let (tx, rx) = sync_channel::<Message>(CHANNEL_DEPTH);
        let client = self.client.clone();
        let policy = self.policy;

        let worker = std::thread::Builder::new()
            .name("teton-model-fetch".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        let _ = tx.send(Message::Failed(FetchError::ClientInit));
                        return;
                    }
                };
                runtime.block_on(stream_into(client, target, offset, policy, &tx));
            })
            .map_err(|err| FetchFailure::Sink(DownloadError::Io(err)))?;

        let mut total = None;
        let mut failure = None;
        for message in &rx {
            match message {
                Message::Length(length) => total = Some(length),
                Message::Chunk(bytes) => {
                    if let Err(err) = sink(&bytes) {
                        failure = Some(FetchFailure::Sink(err));
                        break;
                    }
                }
                Message::Failed(err) => {
                    failure = Some(FetchFailure::Client(err));
                    break;
                }
            }
        }
        // Drop the receiver *before* joining: a worker parked on a send to a
        // full channel only learns the caller is gone when the channel closes.
        drop(rx);
        let _ = worker.join();

        match failure {
            Some(failure) => Err(failure),
            // A worker that ended without reporting a length never got a
            // response — treat it as an aborted transfer rather than inventing
            // a total the caller would trust.
            None => total.ok_or(FetchFailure::Client(FetchError::Network {
                class: "aborted",
            })),
        }
    }
}

impl RangeFetcher for HttpRangeFetcher {
    fn fetch(
        &self,
        url: &str,
        offset: u64,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
    ) -> Result<u64, DownloadError> {
        match self.fetch_inner(url, offset, sink) {
            Ok(total) => {
                self.set_last_error(None);
                Ok(total)
            }
            // The sink's own error is the caller's; it says nothing about the
            // host, so it must not masquerade as this fetcher's last failure.
            Err(FetchFailure::Sink(err)) => Err(err),
            Err(FetchFailure::Client(err)) => {
                let mapped = err.to_download_error();
                self.set_last_error(Some(err));
                Err(mapped)
            }
        }
    }
}

/// Parse and vet a model URL.
///
/// Rejects anything that is not `http(s)`, and — the D-2 rule that matters —
/// anything carrying userinfo credentials. `reqwest` silently converts
/// `https://user:pass@host/…` into an `Authorization: Basic` header, so a
/// base-URL override (BR-16) that smuggled userinfo would put a secret on the
/// wire from the client whose whole contract is that it carries none. Refusing
/// is fail-closed; stripping would quietly change the URL the catalog pinned.
fn validate_url(url: &str) -> Result<reqwest::Url, FetchError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| FetchError::InvalidUrl)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(FetchError::InvalidUrl);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(FetchError::Credentialed);
    }
    Ok(parsed)
}

/// Issue the (possibly retried) request and stream its body into `tx`.
async fn stream_into(
    client: reqwest::Client,
    url: reqwest::Url,
    offset: u64,
    policy: RetryPolicy,
    tx: &SyncSender<Message>,
) {
    let response = match send_with_retry(&client, &url, offset, policy).await {
        Ok(response) => response,
        Err(err) => {
            let _ = tx.send(Message::Failed(err));
            return;
        }
    };
    let (declared, mut skip) = match interpret_head(&response, offset) {
        Ok(head) => head,
        Err(err) => {
            let _ = tx.send(Message::Failed(err));
            return;
        }
    };
    if let Some(total) = declared {
        if tx.send(Message::Length(total)).is_err() {
            return;
        }
    }

    let mut delivered = 0u64;
    let mut body = response.bytes_stream();
    while let Some(item) = body.next().await {
        let bytes = match item {
            Ok(bytes) => bytes,
            Err(err) => {
                let _ = tx.send(Message::Failed(classify(&err)));
                return;
            }
        };
        let mut chunk = bytes.as_ref();
        if skip > 0 {
            let drop_count = usize::try_from(skip).unwrap_or(usize::MAX).min(chunk.len());
            chunk = &chunk[drop_count..];
            skip -= drop_count as u64;
        }
        if chunk.is_empty() {
            continue;
        }
        delivered += chunk.len() as u64;
        if tx.send(Message::Chunk(chunk.to_vec())).is_err() {
            return;
        }
    }

    if declared.is_none() {
        // A length-less (chunked) response still owes the caller a total.
        let _ = tx.send(Message::Length(offset + delivered));
    }
}

/// Send the range request, walking the retry ladder on `429`/`5xx` and on a
/// transient network failure (BR-16).
///
/// Retries happen here — *before* any byte has been handed to the sink — and
/// nowhere else: once the body is streaming, a failure is reported so the
/// orchestrator can resume from the durably-written offset instead of
/// re-delivering bytes the caller already wrote.
async fn send_with_retry(
    client: &reqwest::Client,
    url: &reqwest::Url,
    offset: u64,
    policy: RetryPolicy,
) -> Result<reqwest::Response, FetchError> {
    let mut attempt = 0u32;
    loop {
        let mut request = client.get(url.clone());
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
        }
        match request.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let attempts = attempt + 1;
                let class = backoff::retry_class(status);
                match class {
                    RetryClass::Success => return Ok(response),
                    RetryClass::Permanent => return Err(FetchError::Http { status }),
                    RetryClass::RateLimited if attempt >= policy.max_retries => {
                        return Err(FetchError::RateLimited { status, attempts })
                    }
                    RetryClass::Unavailable if attempt >= policy.max_retries => {
                        return Err(FetchError::Unavailable { status, attempts })
                    }
                    _ => {
                        let retry_after = response
                            .headers()
                            .get(RETRY_AFTER)
                            .and_then(|value| value.to_str().ok())
                            .and_then(backoff::parse_retry_after);
                        tokio::time::sleep(policy.delay(attempt, retry_after)).await;
                        attempt += 1;
                    }
                }
            }
            Err(err) => {
                let classified = classify(&err);
                if !classified.is_transient() || attempt >= policy.max_retries {
                    return Err(classified);
                }
                tokio::time::sleep(policy.delay(attempt, None)).await;
                attempt += 1;
            }
        }
    }
}

/// Read the total length and the number of leading bytes to discard from a
/// successful response.
///
/// A `206` carries the authoritative total in `Content-Range`. A `200` means the
/// host **ignored** our `Range` header and is replaying the resource from byte
/// zero — appending that to a partial file would duplicate bytes and fail the
/// checksum, so the leading `offset` bytes are discarded instead. That is a real
/// mirror behaviour, not a hypothetical.
fn interpret_head(
    response: &reqwest::Response,
    offset: u64,
) -> Result<(Option<u64>, u64), FetchError> {
    if response.status().as_u16() == 206 {
        let header = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .ok_or(FetchError::BadRange)?;
        let (first, total) = parse_content_range(header).ok_or(FetchError::BadRange)?;
        // A range starting *past* our offset would leave a hole in the file.
        if first > offset {
            return Err(FetchError::BadRange);
        }
        return Ok((total, offset - first));
    }
    Ok((response.content_length(), offset))
}

/// Parse `Content-Range: bytes <first>-<last>/<total>` into `(first, total)`.
/// An unknown total (`/*`) yields `None` for the total, which is legal and
/// simply defers the length to the end of the stream.
fn parse_content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let first = range.trim().split('-').next()?.trim().parse().ok()?;
    let total = total.trim();
    let total = if total == "*" {
        None
    } else {
        Some(total.parse().ok()?)
    };
    Some((first, total))
}

/// Reduce a `reqwest` failure to a classified, content-free [`FetchError`].
fn classify(error: &reqwest::Error) -> FetchError {
    if error.is_redirect() {
        FetchError::TooManyRedirects
    } else if error.is_timeout() {
        FetchError::Network { class: "timeout" }
    } else if error.is_connect() {
        FetchError::Network { class: "connect" }
    } else if error.is_body() || error.is_decode() {
        FetchError::Network { class: "body" }
    } else {
        FetchError::Network { class: "io" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;

    use teton_inference::{Downloader, ModelEntry, TierBand};

    /// The fixture artifact: 4096 deterministic bytes. Its digest is pinned
    /// below rather than computed, because the crate's SHA-256 helper is
    /// private — a wrong digest would fail the verify step loudly, so the
    /// constant cannot silently mask a broken download.
    pub(super) fn payload() -> Vec<u8> {
        (0..4096u32).map(|i| (i % 251) as u8).collect()
    }

    /// `sha256` of [`payload`] (`python3 -c "…hashlib.sha256(…)"`).
    const PAYLOAD_SHA256: &str = "d67c656e01756650d77717b0839985a056ec28ffe174601d690fc407a2ceffca";

    /// A catalog entry for `payload()` served from `url`.
    ///
    /// The `revision` is a well-formed placeholder: these tests exercise the
    /// *transport*, and the catalog's own revision-pinning rules (BR-15) are
    /// asserted where they belong, in `teton_inference::catalog`.
    pub(super) fn model_entry(url: &str) -> ModelEntry {
        ModelEntry {
            name: "test-model".to_owned(),
            url: url.to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            sha256: PAYLOAD_SHA256.to_owned(),
            size_bytes: payload().len() as u64,
            ram_floor_bytes: 0,
            band: TierBand::Small,
        }
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("teton-fetch-{tag}-{}.part", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// A retry policy with millisecond delays, so a ladder test finishes in
    /// milliseconds instead of seconds.
    fn fast_policy(max_retries: u32) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            jitter: true,
        }
    }

    // ---------------------------------------------------------------- server

    /// One request as the test server saw it on the wire.
    #[derive(Clone, Debug)]
    pub(super) struct Recorded {
        pub method: String,
        pub target: String,
        pub headers: Vec<(String, String)>,
    }

    impl Recorded {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }

        /// The first byte requested by a `Range: bytes=N-` header, if any.
        pub fn range_start(&self) -> Option<u64> {
            self.header("range")?
                .trim()
                .strip_prefix("bytes=")?
                .split('-')
                .next()?
                .parse()
                .ok()
        }
    }

    type Handler = Arc<dyn Fn(&Recorded, usize) -> Vec<u8> + Send + Sync>;

    /// A minimal HTTP/1.1 server that answers with raw, test-authored bytes.
    ///
    /// Raw responses on purpose: these tests need to produce a truncated body, a
    /// `206` with a hand-written `Content-Range`, and a `429` with a
    /// `Retry-After` — shapes a convenience server would smooth over. It binds
    /// an ephemeral loopback port and records every request it saw, which is how
    /// the credential-free posture is *asserted* rather than assumed.
    pub(super) struct TestServer {
        addr: SocketAddr,
        seen: Arc<Mutex<Vec<Recorded>>>,
        stop: Arc<AtomicBool>,
        accepting: Option<JoinHandle<()>>,
    }

    impl TestServer {
        /// Start a server whose `handler` maps `(request, sequence)` to the raw
        /// response bytes.
        pub fn start<F>(handler: F) -> Self
        where
            F: Fn(&Recorded, usize) -> Vec<u8> + Send + Sync + 'static,
        {
            let handler: Handler = Arc::new(handler);
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
            let addr = listener.local_addr().expect("test server address");
            let seen: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let accepting = std::thread::spawn({
                let seen = Arc::clone(&seen);
                let stop = Arc::clone(&stop);
                move || {
                    for incoming in listener.incoming() {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        let Ok(mut stream) = incoming else { break };
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let Some(request) = read_request(&mut stream) else {
                            continue;
                        };
                        let sequence = {
                            let mut seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
                            seen.push(request.clone());
                            seen.len() - 1
                        };
                        let response = handler(&request, sequence);
                        let _ = stream.write_all(&response);
                        let _ = stream.flush();
                    }
                }
            });
            Self {
                addr,
                seen,
                stop,
                accepting: Some(accepting),
            }
        }

        /// A URL on this server, addressed by IP.
        pub fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.addr)
        }

        /// A URL on this server addressed by the `localhost` **hostname** — a
        /// different host string from [`TestServer::url`], which is what makes a
        /// redirect between the two a cross-host redirect.
        pub fn hostname_url(&self, path: &str) -> String {
            format!("http://localhost:{}{path}", self.addr.port())
        }

        pub fn requests(&self) -> Vec<Recorded> {
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // Wake the blocked `accept` so the thread observes `stop`.
            let _ = TcpStream::connect(self.addr);
            if let Some(accepting) = self.accepting.take() {
                let _ = accepting.join();
            }
        }
    }

    /// Read one request head (and drain any declared body, so closing the
    /// socket cannot reset a client that is still writing).
    fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).ok()?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next()?.to_owned();
        let target = parts.next()?.to_owned();

        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                break;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
            }
        }
        if let Some((_, length)) = headers.iter().find(|(name, _)| name == "content-length") {
            if let Ok(length) = length.parse::<usize>() {
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).ok()?;
            }
        }
        Some(Recorded {
            method,
            target,
            headers,
        })
    }

    fn response_with(
        status: u16,
        reason: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
        for (name, value) in headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str("connection: close\r\n\r\n");
        let mut out = out.into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// `200 OK` with the whole body.
    pub(super) fn ok_response(body: &[u8]) -> Vec<u8> {
        response_with(
            200,
            "OK",
            &[("content-length", body.len().to_string())],
            body,
        )
    }

    /// `206 Partial Content` for `body`, which starts at `first` of `total`.
    fn partial_response(body: &[u8], first: u64, total: u64) -> Vec<u8> {
        let last = first + body.len() as u64 - 1;
        response_with(
            206,
            "Partial Content",
            &[
                ("content-length", body.len().to_string()),
                ("content-range", format!("bytes {first}-{last}/{total}")),
            ],
            body,
        )
    }

    /// A `200` that declares the full length but writes only `written` bytes and
    /// then closes — a connection dropped mid-transfer.
    fn truncated_response(body: &[u8], written: usize) -> Vec<u8> {
        response_with(
            200,
            "OK",
            &[("content-length", body.len().to_string())],
            &body[..written],
        )
    }

    /// A bare status response with no body.
    fn status_response(status: u16, reason: &str, headers: &[(&str, String)]) -> Vec<u8> {
        response_with(status, reason, headers, &[])
    }

    /// `302 Found` pointing at `location`.
    pub(super) fn redirect_response(location: &str) -> Vec<u8> {
        status_response(302, "Found", &[("location", location.to_owned())])
    }

    /// Header names that would mean this client carries a credential. Asserted
    /// against every request every test server saw (D-2, AC-11).
    const CREDENTIAL_HEADERS: &[&str] = &[
        "authorization",
        "proxy-authorization",
        "x-api-key",
        "api-key",
        "x-auth-token",
        "x-amz-security-token",
        "cookie",
    ];

    /// Assert that no request carried anything credential-shaped.
    pub(super) fn assert_no_credential_headers(requests: &[Recorded]) {
        assert!(
            !requests.is_empty(),
            "no requests recorded — the assertion would be vacuous"
        );
        for request in requests {
            for (name, value) in &request.headers {
                assert!(
                    !CREDENTIAL_HEADERS.contains(&name.as_str()),
                    "the model-download client must never send `{name}`"
                );
                assert!(
                    !name.contains("auth") && !name.contains("token") && !name.contains("api-key"),
                    "the model-download client sent a credential-shaped header `{name}`"
                );
                assert!(
                    !value.to_ascii_lowercase().contains("bearer "),
                    "a bearer token appeared in header `{name}`"
                );
            }
        }
    }

    // ----------------------------------------------------------------- tests

    #[test]
    fn downloads_and_verifies_a_model_through_the_real_client() {
        let payload = payload();
        let server = TestServer::start({
            let payload = payload.clone();
            move |request, _| match request.range_start() {
                Some(start) => {
                    partial_response(&payload[start as usize..], start, payload.len() as u64)
                }
                None => ok_response(&payload),
            }
        });
        let fetcher = HttpRangeFetcher::new().expect("build fetcher");
        let dest = temp_path("full");
        let model = model_entry(&server.url("/model.gguf"));

        Downloader::new(&fetcher)
            .fetch(&model, &dest, &mut |_| {})
            .expect("the library's download/verify loop succeeds over HTTP");

        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].target, "/model.gguf");
        assert_no_credential_headers(&requests);
        assert_eq!(fetcher.last_error(), None);
        std::fs::remove_file(&dest).ok();
    }

    #[test]
    fn resumes_a_partial_file_with_a_range_request() {
        let payload = payload();
        let server = TestServer::start({
            let payload = payload.clone();
            move |request, _| match request.range_start() {
                Some(start) => {
                    partial_response(&payload[start as usize..], start, payload.len() as u64)
                }
                None => ok_response(&payload),
            }
        });
        let dest = temp_path("resume-partial");
        // A prior run left the first 2048 bytes on disk.
        std::fs::write(&dest, &payload[..2048]).unwrap();

        let fetcher = HttpRangeFetcher::new().expect("build fetcher");
        Downloader::new(&fetcher)
            .fetch(&model_entry(&server.url("/model.gguf")), &dest, &mut |_| {})
            .expect("resumes from the partial file");

        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        let requests = server.requests();
        assert_eq!(requests.len(), 1, "a resume is a single ranged request");
        assert_eq!(
            requests[0].range_start(),
            Some(2048),
            "the fetcher must ask for the remainder, not the whole file"
        );
        std::fs::remove_file(&dest).ok();
    }

    #[test]
    fn resumes_after_the_connection_drops_mid_stream() {
        let payload = payload();
        let server = TestServer::start({
            let payload = payload.clone();
            move |request, sequence| match request.range_start() {
                Some(start) => {
                    partial_response(&payload[start as usize..], start, payload.len() as u64)
                }
                // The first attempt promises the whole file and delivers a
                // third of it before the connection dies.
                None if sequence == 0 => truncated_response(&payload, 1500),
                None => ok_response(&payload),
            }
        });
        let dest = temp_path("resume-drop");
        let fetcher = HttpRangeFetcher::new().expect("build fetcher");

        let mut progress = Vec::new();
        Downloader::new(&fetcher)
            .fetch(
                &model_entry(&server.url("/model.gguf")),
                &dest,
                &mut |event| {
                    progress.push(event);
                },
            )
            .expect("resumes across the dropped connection");

        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "one interrupted fetch plus one resume");
        assert_eq!(requests[0].range_start(), None);
        assert_eq!(
            requests[1].range_start(),
            Some(1500),
            "the resume must continue from the durably-written bytes"
        );
        assert!(!progress.is_empty(), "progress events are reported");
        // The interruption is transient, and the successful resume clears it.
        assert_eq!(fetcher.last_error(), None);
        std::fs::remove_file(&dest).ok();
    }

    #[test]
    fn a_host_that_ignores_the_range_header_does_not_duplicate_bytes() {
        let payload = payload();
        // This server replays the whole resource for every request, Range or not
        // — a real behaviour of some mirrors and of naive object stores.
        let server = TestServer::start({
            let payload = payload.clone();
            move |_, _| ok_response(&payload)
        });
        let dest = temp_path("range-ignored");
        std::fs::write(&dest, &payload[..2048]).unwrap();

        let fetcher = HttpRangeFetcher::new().expect("build fetcher");
        Downloader::new(&fetcher)
            .fetch(&model_entry(&server.url("/model.gguf")), &dest, &mut |_| {})
            .expect("completes without duplicating the bytes already on disk");

        // Byte-exact: had the leading 2048 bytes not been discarded the file
        // would be 6144 bytes and the checksum would have failed.
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        std::fs::remove_file(&dest).ok();
    }

    #[test]
    fn a_rate_limited_host_is_retried_with_backoff_and_then_succeeds() {
        let payload = payload();
        let server = TestServer::start({
            let payload = payload.clone();
            move |_, sequence| match sequence {
                // Two 429s: one telling us how long to wait, one leaving it to
                // the backoff ladder.
                0 => status_response(429, "Too Many Requests", &[("retry-after", "0".to_owned())]),
                1 => status_response(429, "Too Many Requests", &[]),
                _ => ok_response(&payload),
            }
        });
        let dest = temp_path("rate-limited");
        let fetcher = HttpRangeFetcher::with_policy(fast_policy(3)).expect("build fetcher");

        Downloader::new(&fetcher)
            .fetch(&model_entry(&server.url("/model.gguf")), &dest, &mut |_| {})
            .expect("the retry ladder rides out the rate limit");

        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        assert_eq!(server.requests().len(), 3, "two 429s then the artifact");
        assert_eq!(fetcher.last_error(), None);
        std::fs::remove_file(&dest).ok();
    }

    #[test]
    fn a_persistent_rate_limit_is_reported_as_a_rate_limit_not_a_corrupt_download() {
        let server = TestServer::start(|_, _| status_response(429, "Too Many Requests", &[]));
        let fetcher = HttpRangeFetcher::with_policy(fast_policy(2)).expect("build fetcher");

        let err = fetcher
            .fetch(&server.url("/model.gguf"), 0, &mut |_| Ok(()))
            .expect_err("a permanently rate-limited host must fail");

        // AC-12: the rate limit is its own condition, never confused with the
        // corrupt-artifact error the library's verifier raises.
        assert!(
            !matches!(err, DownloadError::Checksum { .. }),
            "a rate limit must never surface as a corrupt download"
        );
        assert!(
            matches!(err, DownloadError::Transport(_)),
            "a rate limit is transient, so the orchestrator may resume later"
        );
        assert_eq!(
            fetcher.last_error(),
            Some(FetchError::RateLimited {
                status: 429,
                attempts: 3,
            })
        );
        assert!(fetcher.last_error().unwrap().is_rate_limited());
        // The whole ladder was walked: the first attempt plus `max_retries`.
        assert_eq!(server.requests().len(), 3);
    }

    #[test]
    fn a_persistently_unavailable_host_is_reported_as_availability_not_rate_limit() {
        let server = TestServer::start(|_, _| status_response(503, "Service Unavailable", &[]));
        let fetcher = HttpRangeFetcher::with_policy(fast_policy(1)).expect("build fetcher");

        let err = fetcher
            .fetch(&server.url("/model.gguf"), 0, &mut |_| Ok(()))
            .expect_err("an unavailable host must fail");

        assert!(matches!(err, DownloadError::Transport(_)));
        let last = fetcher
            .last_error()
            .expect("a classified cause is retained");
        assert_eq!(
            last,
            FetchError::Unavailable {
                status: 503,
                attempts: 2,
            }
        );
        // Availability and rate limiting stay distinguishable (AC-12).
        assert!(!last.is_rate_limited());
        assert!(last.is_transient());
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn a_permanent_http_failure_aborts_immediately() {
        let server = TestServer::start(|_, _| status_response(404, "Not Found", &[]));
        let fetcher = HttpRangeFetcher::with_policy(fast_policy(3)).expect("build fetcher");

        let err = fetcher
            .fetch(&server.url("/missing.gguf"), 0, &mut |_| Ok(()))
            .expect_err("a 404 must fail");

        // Not `Transport`: the orchestrator must stop rather than resume into a
        // URL that will never exist.
        assert!(
            !matches!(err, DownloadError::Transport(_)),
            "a permanent failure must not look resumable"
        );
        assert!(err.to_string().contains("404"), "got: {err}");
        assert_eq!(fetcher.last_error(), Some(FetchError::Http { status: 404 }));
        assert_eq!(
            server.requests().len(),
            1,
            "a permanent status must not burn the retry ladder"
        );
    }

    #[test]
    fn the_total_length_is_reported_from_the_response() {
        let payload = payload();
        let server = TestServer::start({
            let payload = payload.clone();
            move |request, _| match request.range_start() {
                Some(start) => {
                    partial_response(&payload[start as usize..], start, payload.len() as u64)
                }
                None => ok_response(&payload),
            }
        });
        let fetcher = HttpRangeFetcher::new().expect("build fetcher");

        let mut seen = 0u64;
        let total = fetcher
            .fetch(&server.url("/model.gguf"), 0, &mut |chunk| {
                seen += chunk.len() as u64;
                Ok(())
            })
            .expect("fetch succeeds");
        assert_eq!(total, payload.len() as u64);
        assert_eq!(seen, payload.len() as u64);

        // A ranged fetch reports the *total*, not the length of the range.
        let mut tail = 0u64;
        let total_from_range = fetcher
            .fetch(&server.url("/model.gguf"), 3000, &mut |chunk| {
                tail += chunk.len() as u64;
                Ok(())
            })
            .expect("ranged fetch succeeds");
        assert_eq!(total_from_range, payload.len() as u64);
        assert_eq!(tail, payload.len() as u64 - 3000);
    }

    #[test]
    fn a_sink_error_is_passed_through_untouched() {
        let payload = payload();
        let server = TestServer::start({
            let payload = payload.clone();
            move |_, _| ok_response(&payload)
        });
        let fetcher = HttpRangeFetcher::new().expect("build fetcher");

        let err = fetcher
            .fetch(&server.url("/model.gguf"), 0, &mut |_| {
                Err(DownloadError::Oversized {
                    expected: 1,
                    actual: 2,
                })
            })
            .expect_err("the sink's refusal propagates");

        assert!(matches!(err, DownloadError::Oversized { .. }));
        // The host did nothing wrong, so nothing is blamed on it.
        assert_eq!(fetcher.last_error(), None);
    }

    #[test]
    fn a_url_that_embeds_credentials_is_refused_before_any_request() {
        // `reqwest` would turn userinfo into an `Authorization: Basic` header,
        // which is exactly what this client promises never to send (D-2).
        let fetcher = HttpRangeFetcher::new().expect("build fetcher");
        let err = fetcher
            .fetch(
                "https://user:secret@models.invalid/model.gguf",
                0,
                &mut |_| Ok(()),
            )
            .expect_err("a credentialed URL must be refused");
        assert!(!matches!(err, DownloadError::Transport(_)));
        assert_eq!(fetcher.last_error(), Some(FetchError::Credentialed));
    }

    #[test]
    fn non_http_and_malformed_urls_are_refused() {
        let fetcher = HttpRangeFetcher::new().expect("build fetcher");
        for url in ["file:///etc/passwd", "ftp://host/model.gguf", "not a url"] {
            fetcher
                .fetch(url, 0, &mut |_| Ok(()))
                .expect_err("must refuse");
            assert_eq!(
                fetcher.last_error(),
                Some(FetchError::InvalidUrl),
                "url: {url}"
            );
        }
        // A plain https URL parses fine (no request is made here).
        assert!(validate_url("https://models.example/model.gguf").is_ok());
        assert!(validate_url("http://127.0.0.1:8080/model.gguf").is_ok());
    }

    #[test]
    fn content_range_parsing_covers_the_shapes_hosts_actually_send() {
        assert_eq!(
            parse_content_range("bytes 100-999/1000"),
            Some((100, Some(1000)))
        );
        assert_eq!(parse_content_range("bytes 0-0/1"), Some((0, Some(1))));
        // An unknown total is legal and simply defers the length.
        assert_eq!(parse_content_range("bytes 5-9/*"), Some((5, None)));
        assert_eq!(parse_content_range("bytes */1000"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
        assert_eq!(parse_content_range("bytes 0-1"), None);
        assert_eq!(parse_content_range(""), None);
    }

    #[test]
    fn transient_and_permanent_failures_map_onto_the_right_seam_variant() {
        // Transient: `Transport`, which the orchestrator resumes from.
        for error in [
            FetchError::RateLimited {
                status: 429,
                attempts: 3,
            },
            FetchError::Unavailable {
                status: 503,
                attempts: 3,
            },
            FetchError::Network { class: "connect" },
        ] {
            assert!(error.is_transient());
            assert!(matches!(
                error.to_download_error(),
                DownloadError::Transport(_)
            ));
        }
        // Permanent: anything but `Transport`, so the download stops.
        for error in [
            FetchError::Http { status: 403 },
            FetchError::TooManyRedirects,
            FetchError::InvalidUrl,
            FetchError::Credentialed,
            FetchError::BadRange,
            FetchError::ClientInit,
        ] {
            assert!(!error.is_transient());
            assert!(!matches!(
                error.to_download_error(),
                DownloadError::Transport(_)
            ));
        }
    }

    #[test]
    fn fetch_errors_never_carry_the_url_or_any_content() {
        // Conventions: nothing content-bearing in an error that may be logged.
        let rendered = [
            FetchError::RateLimited {
                status: 429,
                attempts: 2,
            }
            .to_string(),
            FetchError::Http { status: 404 }.to_string(),
            FetchError::Credentialed.to_string(),
            FetchError::Network { class: "connect" }.to_string(),
        ];
        for message in rendered {
            assert!(!message.contains("://"), "leaked a URL: {message}");
            assert!(!message.contains("secret"), "leaked a secret: {message}");
        }
    }

    #[test]
    fn the_fetcher_is_shareable_across_threads() {
        // The daemon holds one fetcher behind an `Arc`; make that a compile-time
        // fact rather than a hope.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<HttpRangeFetcher>();
    }
}

/// The D-2 posture test: the two HTTP clients, asserted **together**.
///
/// AC-11 asks for exactly this pairing. The download client must follow a
/// cross-host redirect (a model host hands off to its CDN) and must carry no
/// credential; the egress client must keep refusing the very same redirect,
/// because it *does* carry credentials and `reqwest` would forward a custom
/// header like `x-api-key` across the hop. Asserting both in one module is the
/// safeguard: a future change that relaxes either client's posture — "just
/// allow redirects", "just reuse the transport" — fails here.
#[cfg(test)]
mod two_client_posture {
    use super::tests::{
        assert_no_credential_headers, ok_response, payload, redirect_response, TestServer,
    };
    use super::*;

    use teton_providers::transport::{HttpMethod, Transport, TransportRequest};

    use crate::egress::HttpTransport;

    /// A pair of servers wired as `origin` → `cdn`, redirecting across a host
    /// change (IP-addressed origin, hostname-addressed CDN).
    ///
    /// The hop is a genuine host change — `127.0.0.1` → `localhost` — which is
    /// what makes it a fair stand-in for a model host handing off to its CDN. It
    /// assumes only that `localhost` reaches IPv4 loopback, directly or by the
    /// connector's fallback after an IPv6 attempt is refused.
    fn redirect_pair() -> (TestServer, TestServer, String) {
        let body = payload();
        let cdn = TestServer::start(move |_, _| ok_response(&body));
        let cdn_url = cdn.hostname_url("/blob.gguf");
        let origin = TestServer::start({
            let cdn_url = cdn_url.clone();
            move |_, _| redirect_response(&cdn_url)
        });
        let origin_url = origin.url("/repo/resolve/abc123/model.gguf");
        (origin, cdn, origin_url)
    }

    #[test]
    fn the_download_client_follows_a_cross_host_redirect_and_carries_no_credential() {
        let (origin, cdn, origin_url) = redirect_pair();
        let fetcher = HttpRangeFetcher::new().expect("build fetcher");

        let mut received = Vec::new();
        let total = fetcher
            .fetch(&origin_url, 0, &mut |chunk| {
                received.extend_from_slice(chunk);
                Ok(())
            })
            .expect("the download client must follow the handoff to the CDN");

        assert_eq!(received, payload(), "the CDN's bytes arrived");
        assert_eq!(total, payload().len() as u64);
        assert_eq!(origin.requests().len(), 1, "the origin was asked first");
        assert_eq!(cdn.requests().len(), 1, "the redirect was followed");
        assert_eq!(cdn.requests()[0].method, "GET");
        assert_eq!(cdn.requests()[0].target, "/blob.gguf");

        // The redirect crossed a host boundary, mimicking model host → CDN.
        let origin_host = origin.requests()[0].header("host").unwrap().to_owned();
        let cdn_host = cdn.requests()[0].header("host").unwrap().to_owned();
        assert_ne!(
            origin_host, cdn_host,
            "the two hops must be different hosts for this to prove anything"
        );

        // …and neither hop saw a credential. This is the assertion that makes
        // following redirects safe here (D-2).
        assert_no_credential_headers(&origin.requests());
        assert_no_credential_headers(&cdn.requests());
    }

    #[tokio::test]
    async fn the_egress_client_still_refuses_the_same_redirect() {
        let (origin, cdn, origin_url) = redirect_pair();
        let transport = HttpTransport::new().expect("build the egress transport");

        let response = transport
            .execute(TransportRequest {
                method: HttpMethod::Post,
                url: origin_url,
                headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                body: b"{}".to_vec(),
            })
            .await
            .expect("the 3xx is returned to the caller, not followed");

        // The caller sees the redirect itself…
        assert_eq!(
            response.status, 302,
            "egress must surface the 3xx instead of following it"
        );
        assert_eq!(origin.requests().len(), 1);
        // …and the second host is never contacted, so a credential bound to the
        // first host can never ride along to it.
        assert!(
            cdn.requests().is_empty(),
            "egress followed a cross-host redirect — a credential could now leak \
             (reqwest strips `Authorization` across hosts but NOT `x-api-key`)"
        );
    }
}
