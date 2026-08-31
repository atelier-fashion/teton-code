//! REQ-599 step 7: reaching a remote provider — transport, credentials, probe.
//!
//! `build_remote_transport`, `provider_auth_headers` and `build_provider`, plus
//! the connection probe (`stream_probe`, `probe_outcome`, `ProbeAnswer`) and the
//! health translation the router reads.
//!
//! **This is ADR-4's step 4, and it could not be done at step 4.** The census
//! then measured the `provider` group's items spanning 10,366 lines — not a
//! seam, just a name. Extracting the config document, the duty family, the
//! engine and the views left what remained *contiguous*: 375 lines, a real
//! cluster. Seams are not only discovered, they are also **created** by earlier
//! extractions, which is a fact about decomposition order that ADR-4's
//! cheapest-first rule half-anticipated and did not say outright.

use super::*;

/// The `model_id` a lifecycle event carries when the machine has no model to
/// name — a below-the-floor probe, or a catalog with nothing that fits.
pub(crate) const LOCAL_TIER_ID: &str = "local";

/// The Anthropic Messages API version header value the credential layer injects
/// alongside `x-api-key` (mirrors the adapter's protocol header; the injected
/// copy wins so no duplicate reaches the wire).
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Build the endpoint-bound HTTP transport for a remote provider turn (BR-7,
/// REQ-544 M-3).
///
/// A provider with no `auth_ref` gets a credential-free transport (the e2e mock
/// endpoints and any keyless provider). Otherwise the `auth_ref` is resolved to a
/// secret, turned into the provider-appropriate credential header(s), and bound
/// to the provider's endpoint origin so the header can never ride an MCP or
/// cross-provider request. A resolution failure is a typed
/// [`HarnessError::Credential`] — never a panic, and its message never carries
/// the secret.
pub(crate) fn build_remote_transport(
    provider: &ModelProvider,
    resolver: &SecretResolver,
) -> Result<HttpTransport, HarnessError> {
    match provider.auth_ref.as_deref() {
        None => HttpTransport::new()
            .map_err(|e| HarnessError::Engine(EngineError::Backend(e.to_string()))),
        Some(auth_ref) => {
            // BR-7 / REQ-544 M-3: an `auth_ref` provider MUST have an endpoint that
            // parses to a tuple (network-addressable) origin, or the resolved
            // credential can never be bound to it — `with_endpoint_auth` would
            // attach the header to nothing (`origin_of` is `None`), the call would
            // 401, and there would be no sign the auth was silently stripped.
            // Reject it loudly as a config/credential error instead. Checked before
            // the keychain is touched, and the message names only the reference
            // (config, safe) — never the secret.
            let endpoint = provider.endpoint.clone().unwrap_or_default();
            if origin_of(&endpoint).is_none() {
                return Err(HarnessError::Credential(format!(
                    "provider `{}` declares auth_ref `{auth_ref}` but its endpoint does not \
                     parse to a network origin; the credential cannot be bound and would be \
                     silently dropped",
                    provider.id
                )));
            }
            let secret = resolver
                .resolve(auth_ref)
                .map_err(|e| HarnessError::Credential(e.to_string()))?;
            let headers = provider_auth_headers(provider.kind, &secret);
            HttpTransport::with_endpoint_auth(&endpoint, headers)
                .map_err(|e| HarnessError::Engine(EngineError::Backend(e.to_string())))
        }
    }
}

/// The provider-appropriate credential header(s) for a resolved `secret` (BR-7).
///
/// Anthropic authenticates with `x-api-key` (plus the `anthropic-version` the
/// API requires); OpenAI-compatible and custom endpoints use a bearer token. The
/// local tier never authenticates. Header *names* are safe to construct here; the
/// secret value lives only in the returned headers and is dropped after the
/// endpoint-bound transport is built — it never reaches a log or `CostRecord`.
pub(crate) fn provider_auth_headers(kind: ProviderKind, secret: &str) -> Vec<(String, String)> {
    match kind {
        ProviderKind::Anthropic => vec![
            ("x-api-key".to_owned(), secret.to_owned()),
            ("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()),
        ],
        ProviderKind::OpenaiCompatible | ProviderKind::Custom => {
            vec![("authorization".to_owned(), format!("Bearer {secret}"))]
        }
        // The local tier does not reach a remote transport, so it needs no auth.
        ProviderKind::Local => Vec::new(),
    }
}

/// Build a concrete [`Provider`] adapter from a config provider entry.
pub(crate) fn build_provider(
    provider: &ModelProvider,
    caps: CapabilityProfile,
) -> Box<dyn Provider> {
    let endpoint = provider.endpoint.clone().unwrap_or_default();
    match provider.kind {
        ProviderKind::Anthropic => {
            Box::new(AnthropicAdapter::new(provider.id.clone(), endpoint).with_capabilities(caps))
        }
        // OpenAI-compatible and custom both speak the OpenAI chat/completions
        // shape in the MVP.
        _ => Box::new(OpenAiCompatAdapter::new(
            OpenAiCompatConfig::new(provider.id.clone(), endpoint).with_capabilities(caps),
        )),
    }
}

// ---------------------------------------------------------------------------
// The connection test's own pieces (REQ-581)
//
// They live here, beside `build_provider` and `build_remote_transport`, because
// that is what the probe *is*: the turn path's constructors, one fixed request,
// and a classifier that keeps the failure typed instead of flattening it to a
// string (ADR-1).
// ---------------------------------------------------------------------------

/// The one sentence a connection test sends (REQ-581 BR-1).
///
/// A constant, and a boring one on purpose: it carries no user content, no file
/// provenance and no conversation, which is what lets the probe's choke point
/// run without a redaction gate (ADR-1) and what makes the *cost* of a test
/// predictable enough to preview before the user consents (BR-2). It asks for
/// one word so that the reply is worthless and the fact of a reply is the whole
/// signal.
pub(crate) const PROBE_PROMPT: &str = "Reply with the single word OK.";

/// The generation budget one connection test asks for (REQ-581 BR-1).
///
/// The floor rather than a default: the test is billed like a turn (BR-5), so
/// the smallest budget that can still produce a completion is the honest one to
/// spend on a user's key. `max_tokens` is a request and not a guarantee — a
/// provider that overruns it is billed for what it sent, which the ledger row
/// and the reported token counts both say.
pub(crate) const PROBE_MAX_TOKENS: u32 = 8;

/// How long one connection test waits for the vendor before it stops (REQ-581
/// verify F3).
///
/// The provider transport carries no timeout at all, and that is right for a
/// *turn*: a model reasoning through a hard request is not a stalled one, and a
/// deadline there would cut real work off mid-thought. A probe is not a turn. It
/// is [`PROBE_MAX_TOKENS`] of budget spent on [`PROBE_PROMPT`], so an endpoint
/// that has produced nothing in thirty seconds is not thinking — it is not
/// answering, which is exactly the fact the user ran the command to learn.
///
/// Thirty seconds rather than something tighter because the figure has to clear
/// a cold TLS handshake to a distant region on a slow link and still be a bound
/// a person will wait out; and because the cost of being wrong is asymmetric —
/// a deadline hit early reports `unreachable` about a provider that works.
pub(crate) const PROBE_DEADLINE: Duration = Duration::from_secs(30);

/// What draining one probe stream turned out to have been (REQ-581 verify F1).
///
/// The distinction exists because "the transport did not error" is a much weaker
/// fact than it looks. Both adapters raise a [`ProviderError`] only at a status
/// of 400 or above, and `event_stream`'s tail synthesizes a terminal `Completed`
/// even for a body that held no `data:` lines at all — so a 301 (redirects are
/// not followed), a 204, or a 200 carrying ordinary JSON all arrive here as a
/// completion of zero tokens. Reporting those as `Reached { 0 in / 0 out }` and
/// stamping the provider healthy would be the connection test's worst possible
/// failure: a green answer for an endpoint no turn can use.
///
/// [`Self::NotACompletion`] is reported as
/// [`ProviderTestOutcome::NotACompletion`] and not as an `unreachable`: a host
/// that answered is a different fact, and a different next move, from one that
/// never did (BR-3).
pub(crate) enum ProbeAnswer {
    /// The vendor streamed a completion — text, a tool call, or a non-zero
    /// usage reading. The token counts it reported, which may be zero when a
    /// provider streams text and declines to say what it billed.
    Completion(teton_providers::TokenUsage),
    /// Something answered with a status the adapters accept, and then produced
    /// no completion: no text, no tool call, and no tokens.
    NotACompletion,
}

/// Drive one probe request to the end of its stream and report what came back
/// (REQ-581 BR-1).
///
/// The stream is **drained**, not read: the text is thrown away and only the
/// *fact* of it is kept, a tool call is a provider ignoring a request that
/// offered no tools, and `Completed` carries the token counts. Draining is also
/// what drives the cost meter's wrapped body to write its row, so a probe that
/// returned early would reach the vendor and bill nothing.
///
/// A mid-stream error ends the drain. There is no retry here (ADR-1): a stream
/// that broke has already told the test what it asked.
pub(crate) async fn stream_probe(
    adapter: &dyn Provider,
    request: TurnRequest,
    transport: &dyn Transport,
) -> Result<ProbeAnswer, ProviderError> {
    use futures::StreamExt;

    let mut stream = adapter.stream_turn(request, transport).await?;
    let mut usage = None;
    // Whether anything on this stream was the vendor *generating*. A tool call
    // counts even though the probe offered no tools: it is still a completion
    // being streamed, and reading it as "not a completion" would report an
    // over-eager provider as an unreachable one.
    let mut generated = false;
    while let Some(event) = stream.next().await {
        match event? {
            TurnEvent::TextDelta(text) => generated |= !text.is_empty(),
            TurnEvent::ToolCall(_) => generated = true,
            TurnEvent::Completed(completion) => usage = Some(completion.usage),
        }
    }
    // The `unwrap_or_default` is defensive rather than live — both adapters
    // guarantee a terminal `Completed` — and it is the *zero* case below that
    // this function exists to separate out.
    let usage = usage.unwrap_or_default();
    if !generated && usage.input_tokens == 0 && usage.output_tokens == 0 {
        return Ok(ProbeAnswer::NotACompletion);
    }
    Ok(ProbeAnswer::Completion(usage))
}

/// The typed outcome one failed probe earns (REQ-581 BR-3, architecture ADR-2).
///
/// The whole mapping table, and the whole of what a client needs to branch on:
/// the daemon classifies once, here, and a surface renders the variant rather
/// than re-reading a sentence to work out what happened (LESSON-456).
///
/// | Signal | Outcome |
/// |---|---|
/// | 401, 403 | `Refused`, naming the credential *reference* |
/// | 404 | `UnknownModel`, naming the model the config declares |
/// | 429 | `RateLimited { retry_after_secs: None }` |
/// | other 4xx | `Refused { status }` |
/// | 5xx | `ServerError { status }` |
/// | timeout / transport / malformed | `Unreachable` |
///
/// The two outcomes this function does **not** produce are the two that are not
/// errors at all: [`ProviderTestOutcome::NotACompletion`] (the vendor answered
/// with a status below 400 and completed nothing — [`ProbeAnswer`]'s job) and
/// [`ProviderTestOutcome::TimedOut`] (the probe's own deadline elapsed, so no
/// [`ProviderError`] was ever raised). `ProviderError::Timeout` is a *different*
/// fact — the transport's own verdict — and stays on the `Unreachable` row.
///
/// # Every `reason` is composed from facts this daemon owns (ADR-3)
///
/// The status, the dial host, the model out of the config, and the credential
/// *reference*. Never a response body — a vendor's error body can echo the
/// request back, so pasting one into the transcript would put a third party's
/// prose, and possibly the user's own bytes, where neither belongs. Never a
/// header. Never the credential value: it exists only inside the headers
/// `build_remote_transport` built, and nothing up here has ever seen it.
///
/// # The three arms that cannot occur, and why they are still written
///
/// `EffortRefused` (the probe sends no reasoning field), `PrivacyBlocked`
/// (empty provenance, constant payload, no redaction gate) and `Build` (a local
/// serialization failure) are unreachable through this call path. They are
/// classified anyway rather than swept into a `_` arm: a `_ => fixed_string` is
/// a decision to discard evidence, and the day one of them *does* arrive the
/// fixed string would be actively false rather than merely vague (LESSON-456).
pub(crate) fn probe_outcome(
    err: &ProviderError,
    host: &str,
    model: &str,
    auth_ref: Option<&str>,
) -> ProviderTestOutcome {
    // How the refusal names what the request authenticated with. A provider
    // with no `auth_ref` sends no credential at all, and saying so is the
    // actionable half of a 401 for exactly that configuration.
    let credential = match auth_ref {
        Some(reference) => format!("the vendor did not accept the credential at `{reference}`"),
        None => {
            "this provider declares no `auth_ref`, so the request carried no credential".to_owned()
        }
    };
    match err {
        ProviderError::ClientError { status } => match status {
            401 | 403 => ProviderTestOutcome::Refused {
                status: *status,
                reason: format!("HTTP {status} from `{host}` — {credential}"),
            },
            // The endpoint exists — registration validated its shape and
            // something answered on it — so the missing thing is the model the
            // config declares. A 400 is deliberately NOT read this way: guessing
            // "that was about the model" from a bare 400 is the re-reading of a
            // vendor's prose this classifier exists to avoid.
            404 => ProviderTestOutcome::UnknownModel {
                status: *status,
                reason: format!("`{model}` is not a model `{host}` serves (HTTP 404)"),
            },
            // `retry_after_secs` is `None` in v1 by design: the transport
            // surfaces exactly one named header and a probe does not earn a
            // second (ADR-2, OQ-5 — deferred, not dropped).
            429 => ProviderTestOutcome::RateLimited {
                retry_after_secs: None,
            },
            _ => ProviderTestOutcome::Refused {
                status: *status,
                reason: format!(
                    "HTTP {status} from `{host}` — the vendor answered and would not serve the \
                     call"
                ),
            },
        },
        ProviderError::ServerError { status } => ProviderTestOutcome::ServerError {
            status: *status,
            reason: format!(
                "HTTP {status} from `{host}` — the vendor answered and is failing; the \
                 configuration is not the suspect"
            ),
        },
        ProviderError::Timeout => ProviderTestOutcome::Unreachable {
            reason: format!("could not reach `{host}`: the request timed out"),
        },
        ProviderError::Transport => ProviderTestOutcome::Unreachable {
            reason: format!("could not reach `{host}`: a transport failure (DNS, TCP or TLS)"),
        },
        ProviderError::MalformedResponse => ProviderTestOutcome::Unreachable {
            reason: format!(
                "could not reach `{host}`: a malformed response — something answered, and not \
                 with a completion stream"
            ),
        },
        // Cannot occur: the probe offers no tools. If it ever does, the vendor
        // answered with something this daemon could not read, which is the same
        // fact a malformed response carries.
        ProviderError::MalformedToolCall { .. } => ProviderTestOutcome::Unreachable {
            reason: format!(
                "could not reach `{host}`: a malformed response — the reply carried a tool call \
                 the probe never offered"
            ),
        },
        // Cannot occur: the probe sends no reasoning field, so nothing can be
        // refused for naming one. The status is not invented — this error is
        // only ever minted from a 400 whose body names the field.
        ProviderError::EffortRefused { .. } => ProviderTestOutcome::Refused {
            status: 400,
            reason: format!(
                "HTTP 400 from `{host}` — the vendor refused the reasoning-effort field, which \
                 this test does not send"
            ),
        },
        // REQ-586 TASK-185: the probe's fixed payload cannot exceed a window; folded like EffortRefused
        ProviderError::ContextLengthExceeded { .. } => ProviderTestOutcome::Refused {
            status: 400,
            reason: format!(
                "HTTP 400 from `{host}` — the vendor refused the request as exceeding its \
                 context window, which this test's fixed payload cannot do"
            ),
        },
        // Cannot occur: empty provenance and a constant payload, with no
        // redaction gate installed. Nothing left the machine if it does.
        ProviderError::PrivacyBlocked(_) => ProviderTestOutcome::Unreachable {
            reason: format!(
                "nothing was sent to `{host}`: the egress choke point refused the call"
            ),
        },
        // REQ-588: `/provider test` builds its own `EgressContext` and attaches
        // no prompt accumulator, so the ceiling never binds here — a connection
        // test is not a prompt and must not be refused because some prompt
        // elsewhere spent its budget. Spelled out rather than left to a
        // catch-all, for the reason the arm above gives about its own
        // impossible case.
        // Cannot occur: the probe's egress is built without a spend ceiling on
        // purpose — a connection test is not the user's prompt and must not be
        // refused by that prompt's budget. If one is ever wired in, this is the
        // honest label: `Unreachable` here means the host was not reached, the
        // same sense `Build` below it carries, not that anything is wrong with
        // the vendor.
        ProviderError::SpendCeilingReached => ProviderTestOutcome::Unreachable {
            reason: format!(
                "nothing was sent to `{host}`: the egress choke point refused the call"
            ),
        },
        // A local failure, not the vendor's: the request never became bytes.
        ProviderError::Build(_) => ProviderTestOutcome::Unreachable {
            reason: format!(
                "nothing was sent to `{host}`: the request could not be built on this machine"
            ),
        },
    }
}

/// The wire spelling of the daemon's own [`ProviderHealth`] (REQ-581 BR-4).
///
/// Declared here rather than as a `From` on either type for the reason the two
/// enums are two types at all: `teton-protocol` depends on no other teton
/// crate, so the daemon owns the mapping, exhaustively and in one place.
pub(crate) fn to_protocol_health(health: ProviderHealth) -> WireProviderHealth {
    match health {
        ProviderHealth::Healthy => WireProviderHealth::Healthy,
        ProviderHealth::Degraded => WireProviderHealth::Degraded,
        ProviderHealth::Unavailable => WireProviderHealth::Unavailable,
    }
}

/// REQ-581 TASK-163 — the connection test: one real call, typed on the way
/// back, with the health map and the ledger moved by it.
///
/// # What each layer here is for
///
/// [`probe_outcome`] is a pure function, so ADR-2's mapping table is a
/// *table test* — every row asserted, including the rows that say a
/// credential reference may appear and a secret may not.
///
/// The rest drive [`DaemonRuntime::provider_test`] itself against a
/// loopback HTTP server, because the method builds its own
/// [`HttpTransport`] out of the config and a fake `Transport` cannot be
/// injected into it — substituting one would replace the very object whose
/// credential binding, status handling and metering are the claims. Both
/// ends are `127.0.0.1`; nothing leaves the machine.
#[cfg(test)]
mod provider_test {
    use super::*;
    use crate::keychain::{BackendError, KeychainBackend};
    use teton_protocol::events::EventEnvelope;

    /// A secret **planted** in the resolver, so "no reason ever carries the
    /// key" is a claim about a string that genuinely exists on this path
    /// rather than about an empty credential.
    const PLANTED_SECRET: &str = "sk-planted-never-print-9f3a2b";

    /// The reference the config names — which every 401 reason *must*
    /// carry (AC-2).
    const AUTH_REF: &str = "keychain://teton/kimi";

    /// A keychain that answers exactly the one reference the config names.
    struct FakeKeychain;

    impl KeychainBackend for FakeKeychain {
        fn get(&self, service: &str, account: &str) -> Result<String, BackendError> {
            if service == "teton" && account == "kimi" {
                Ok(PLANTED_SECRET.to_owned())
            } else {
                Err(BackendError::NotFound)
            }
        }
    }

    /// A completed OpenAI-compatible stream: one delta, a usage chunk, and
    /// the terminator — the shape both the adapter's parser and the cost
    /// meter's scanner read, which is why one fixture can prove they agree.
    const REACHED_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\n",
        "data: [DONE]\n\n",
    );

    /// The token counts [`REACHED_SSE`] reports, named once so the
    /// assertions and the price lookup cannot drift from the fixture.
    const REACHED_INPUT: u64 = 12;
    const REACHED_OUTPUT: u64 = 3;

    /// A loopback HTTP server that answers every request with one scripted
    /// response and keeps the head of each request it was sent.
    ///
    /// The request log is what makes "nothing was sent" checkable by
    /// inspection rather than inferred from an error code (LESSON-519): a
    /// refusal that returns before dialing leaves this empty, and a
    /// refactor that dialed first would fill it.
    struct ProbeServer {
        port: u16,
        heads: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl ProbeServer {
        /// Bind a port answering `status`/`body` to every request.
        async fn answering(status: &'static str, content_type: &str, body: String) -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let response = Arc::new(format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            ));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a loopback port");
            let port = listener.local_addr().expect("local addr").port();
            let heads = Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&heads);
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let mut head = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    loop {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => head.extend_from_slice(&chunk[..n]),
                        }
                        if head.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    sink.lock()
                        .expect("capture mutex")
                        .push(String::from_utf8_lossy(&head).into_owned());
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                }
            });
            Self { port, heads }
        }

        /// A server that completes a turn (200 + usage).
        async fn reached() -> Self {
            Self::answering("200 OK", "text/event-stream", REACHED_SSE.to_owned()).await
        }

        /// A loopback port that **accepts and then says nothing**, holding
        /// every connection open for the life of the fixture.
        ///
        /// The honest shape of a hung vendor, and the one a closed port
        /// cannot stand in for: the connect succeeds, the request goes out,
        /// and the response never starts. Nothing here answers, so the only
        /// thing that can end a probe against it is the deadline.
        async fn parked() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a loopback port");
            let port = listener.local_addr().expect("local addr").port();
            let heads = Arc::new(std::sync::Mutex::new(Vec::new()));
            tokio::spawn(async move {
                let mut held = Vec::new();
                while let Ok((socket, _)) = listener.accept().await {
                    // Held rather than dropped: a dropped socket is a reset,
                    // which is `Transport`, which is the outcome this
                    // fixture must NOT be able to produce.
                    held.push(socket);
                }
            });
            Self { port, heads }
        }

        /// A server that answers `status` with a body that **echoes the
        /// planted secret** — the vendor-body-echoes-the-request case ADR-3
        /// exists for.
        async fn refusing(status: &'static str) -> Self {
            Self::answering(
                status,
                "application/json",
                format!("{{\"error\":\"bad key: {PLANTED_SECRET}\"}}"),
            )
            .await
        }

        fn endpoint(&self) -> String {
            format!("http://127.0.0.1:{}/v1/chat/completions", self.port)
        }

        fn requests(&self) -> Vec<String> {
            self.heads.lock().expect("capture mutex").clone()
        }
    }

    /// A runtime whose one remote provider dials `endpoint` for `model`,
    /// with [`AUTH_REF`] resolvable through the fake keychain.
    fn runtime_dialing(endpoint: &str, model: &str) -> DaemonRuntime {
        let mut runtime = DaemonRuntime::minimal();
        {
            let mut config = runtime.config.lock().expect("config mutex");
            config.providers = vec![ModelProvider {
                id: "kimi".to_owned(),
                kind: ProviderKind::OpenaiCompatible,
                endpoint: Some(endpoint.to_owned()),
                model: Some(model.to_owned()),
                auth_ref: Some(AUTH_REF.to_owned()),
                allow_cleartext: false,
                capabilities: ProviderCapabilities::default(),
            }];
        }
        runtime.secret_resolver = SecretResolver::with_backend(Box::new(FakeKeychain));
        runtime
    }

    /// The `provider_tested` envelopes a subscription saw.
    fn tested_events(sub: &mut crate::broadcast::Subscription) -> Vec<EventEnvelope> {
        std::iter::from_fn(|| sub.try_recv())
            .filter(|env| matches!(env.event, Event::ProviderTested(_)))
            .collect()
    }

    /// Exactly one `provider_tested`, scoped to `session`, carrying the
    /// same outcome the caller was handed (BR-3: one value, two readers).
    ///
    /// It **returns** the envelope rather than only asserting on it,
    /// because reading a subscription drains it: a caller that wanted to
    /// inspect the payload afterwards would be inspecting an empty vector
    /// and asserting nothing.
    fn assert_announced_once(
        sub: &mut crate::broadcast::Subscription,
        session: &SessionId,
        result: &ProviderTestResult,
    ) -> EventEnvelope {
        let events = tested_events(sub);
        assert_eq!(
            events.len(),
            1,
            "one test announces exactly one `provider_tested`: {events:?}"
        );
        assert_eq!(
            events[0].session_id.as_ref(),
            Some(session),
            "the announcement is session-scoped, so a second client attached \
             to this session sees routing health change: {:?}",
            events[0]
        );
        let Event::ProviderTested(tested) = &events[0].event else {
            unreachable!("filtered above")
        };
        assert_eq!(tested.outcome, result.outcome);
        assert_eq!(tested.health_after, result.health_after);
        assert_eq!(tested.provider_id, result.provider_id);
        events.into_iter().next().expect("asserted non-empty above")
    }

    // -- ADR-2's table, on the pure classifier ---------------------------

    /// **Every row of ADR-2's mapping table** (AC-2, AC-3, AC-7's typing).
    ///
    /// The classifier is pure, so the table is asserted directly rather
    /// than through nine sockets — and the rows that "cannot occur" are
    /// asserted too, because an unreachable arm that silently became a
    /// `refused { 0 }` would be exactly the discarded evidence LESSON-456
    /// is about.
    #[test]
    fn the_outcome_table_is_the_daemons_own_classification() {
        let host = "api.moonshot.ai";
        let model = "kimi-k3";
        let outcome = |err: ProviderError| probe_outcome(&err, host, model, Some(AUTH_REF));

        for status in [401_u16, 403] {
            let refused = outcome(ProviderError::ClientError { status });
            let ProviderTestOutcome::Refused {
                status: reported,
                reason,
            } = &refused
            else {
                panic!("{status} is a refusal, not {refused:?}");
            };
            assert_eq!(*reported, status);
            assert!(reason.contains(host), "{reason}");
            assert!(
                reason.contains(AUTH_REF),
                "AC-2: a 401 names the credential *reference* the request \
                 authenticated with: {reason}"
            );
            assert!(
                !reason.contains(PLANTED_SECRET),
                "and never the value: {reason}"
            );
        }

        // 404: the endpoint answered, so the missing thing is the model.
        let unknown = outcome(ProviderError::ClientError { status: 404 });
        let ProviderTestOutcome::UnknownModel { status, reason } = &unknown else {
            panic!("404 is the unknown-model outcome, not {unknown:?}");
        };
        assert_eq!(*status, 404);
        assert!(
            reason.contains(model) && reason.contains(host),
            "the sentence names the model that needs fixing and where it was \
             asked: {reason}"
        );

        // 429: typed, and carrying no `Retry-After` in v1 by design.
        assert_eq!(
            outcome(ProviderError::ClientError { status: 429 }),
            ProviderTestOutcome::RateLimited {
                retry_after_secs: None
            },
            "ADR-2 / OQ-5: the header is deferred, not dropped — and a \
             rate limit is never collapsed into a generic refusal"
        );

        // A 400 stays `refused`, deliberately: reading "that was about the
        // model" out of a bare 400 is the guessing this enum exists to
        // avoid.
        for status in [400_u16, 402, 418] {
            let other = outcome(ProviderError::ClientError { status });
            let ProviderTestOutcome::Refused {
                status: reported, ..
            } = other
            else {
                panic!("{status} is a refusal, not {other:?}");
            };
            assert_eq!(reported, status);
        }

        for status in [500_u16, 502, 503] {
            let server = outcome(ProviderError::ServerError { status });
            let ProviderTestOutcome::ServerError {
                status: reported,
                reason,
            } = &server
            else {
                panic!("{status} is a server error, not {server:?}");
            };
            assert_eq!(*reported, status);
            assert!(reason.contains(host), "{reason}");
        }

        for err in [
            ProviderError::Timeout,
            ProviderError::Transport,
            ProviderError::MalformedResponse,
        ] {
            let described = err.to_string();
            let unreachable = outcome(err);
            let ProviderTestOutcome::Unreachable { reason } = &unreachable else {
                panic!("{described} is unreachable, not {unreachable:?}");
            };
            assert!(reason.contains(host), "{reason}");
        }

        // The three that cannot occur on this path are still classified,
        // and none of them collapses onto another row's meaning.
        assert!(matches!(
            outcome(ProviderError::MalformedToolCall {
                tool: "shell".to_owned()
            }),
            ProviderTestOutcome::Unreachable { .. }
        ));
        assert!(matches!(
            outcome(ProviderError::EffortRefused {
                provider_id: "kimi".to_owned(),
                requested: teton_core::EffortLevel::High,
                clamped: teton_core::EffortLevel::High,
            }),
            ProviderTestOutcome::Refused { status: 400, .. }
        ));
        assert!(matches!(
            outcome(ProviderError::Build("serde".to_owned())),
            ProviderTestOutcome::Unreachable { .. }
        ));
    }

    /// A provider with no `auth_ref` sends no credential, and its refusal
    /// says *that* rather than naming a reference it has not got — the
    /// `_ => fixed_string` this classifier does not have.
    #[test]
    fn a_keyless_providers_refusal_says_no_credential_was_sent() {
        let refused = probe_outcome(
            &ProviderError::ClientError { status: 401 },
            "127.0.0.1:8080",
            "local-mock",
            None,
        );
        let ProviderTestOutcome::Refused { reason, .. } = &refused else {
            panic!("{refused:?}");
        };
        assert!(reason.contains("auth_ref"), "{reason}");
        assert!(!reason.contains("credential at"), "{reason}");
    }

    // -- the two refusals that never dial -------------------------------

    /// **AC-7.** A `kind = "local"` provider is refused with the local
    /// tier's own state, and nothing is dialed, metered or announced.
    ///
    /// The sentence is compared against [`DaemonRuntime::unserved_turn_error`]'s
    /// — the daemon's one classifier for what the tier is doing — because
    /// the failure this guards against is a *second* renderer drifting from
    /// the first (LESSON-456), not a typo.
    #[tokio::test]
    async fn a_local_provider_is_refused_with_the_tiers_state_and_never_dialed() {
        let runtime = DaemonRuntime::minimal();
        {
            let mut config = runtime.config.lock().expect("config mutex");
            config.providers = vec![ModelProvider {
                id: "onlocal".to_owned(),
                kind: ProviderKind::Local,
                endpoint: None,
                model: None,
                auth_ref: None,
                allow_cleartext: false,
                capabilities: ProviderCapabilities::default(),
            }];
        }
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let session = SessionId::from("sess");

        let err = runtime
            .provider_test(&bus, &session, &ProviderId::from("onlocal"))
            .await
            .expect_err("a local provider has nothing to dial");

        assert_eq!(err.code, error_code::INVALID_PARAMS);
        let state = {
            let config = runtime.config.lock().expect("config mutex");
            runtime.unserved_turn_error(&config, None).message
        };
        assert!(
            err.message.contains(&state),
            "BR-8: the refusal carries the tier's own state sentence, from \
             the one classifier that owns it.\ngot:  {}\nstate: {state}",
            err.message
        );
        assert!(
            err.message.contains("teton doctor"),
            "and points at the diagnostic that reports it: {}",
            err.message
        );

        // Inspected, not inferred: no call was made and nothing was said.
        assert_eq!(
            runtime.ledger.report().expect("report").total.calls,
            0,
            "a refusal that never dialed must bill nothing"
        );
        assert!(
            tested_events(&mut sub).is_empty(),
            "a refusal that made no call announces nothing (LESSON-513)"
        );
    }

    /// An unknown id names what *is* registered, so the user can see the
    /// spelling rather than guess at it.
    #[tokio::test]
    async fn an_unknown_provider_id_names_the_registered_ids() {
        let runtime = runtime_dialing("https://api.moonshot.ai/v1/chat/completions", "kimi-k3");
        let bus = Arc::new(EventBus::new());
        let session = SessionId::from("sess");

        let err = runtime
            .provider_test(&bus, &session, &ProviderId::from("kimmi"))
            .await
            .expect_err("no such provider");

        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(err.message.contains("kimmi"), "{}", err.message);
        assert!(
            err.message.contains("kimi"),
            "the registered ids are what makes this actionable: {}",
            err.message
        );
    }

    /// A credential that will not resolve is a config problem, not an
    /// outcome — and it is caught before a byte is sent.
    #[tokio::test]
    async fn an_unresolvable_credential_is_refused_before_anything_is_sent() {
        let server = ProbeServer::reached().await;
        let runtime = runtime_dialing(&server.endpoint(), "kimi-k3");
        {
            let mut config = runtime.config.lock().expect("config mutex");
            config.providers[0].auth_ref = Some("keychain://teton/absent".to_owned());
        }
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let session = SessionId::from("sess");

        let err = runtime
            .provider_test(&bus, &session, &ProviderId::from("kimi"))
            .await
            .expect_err("an unresolvable reference is not an outcome");

        assert_eq!(err.code, error_code::CONFIG_REJECTED);
        assert!(
            err.message.contains("keychain://teton/absent")
                && err.message.contains("Nothing was sent"),
            "{}",
            err.message
        );
        assert!(
            server.requests().is_empty(),
            "inspected, not inferred: the server saw {:?}",
            server.requests()
        );
        assert!(tested_events(&mut sub).is_empty());
    }

    // -- through the real method, over a real socket ---------------------

    /// **AC-1 and AC-5 at the unit level.** A completed call reports what
    /// came back, returns an `Unavailable` provider to `Healthy`, writes
    /// exactly one probe row the report counts, and announces once.
    ///
    /// The `usd_micros` assertion is the load-bearing one: it is compared
    /// against the row the **meter** wrote, not against a second price
    /// lookup, so the figure the report shows and the figure `teton cost`
    /// reads back cannot disagree (BR-5).
    #[tokio::test]
    async fn a_reached_test_reports_usage_clears_health_and_records_one_probe() {
        let server = ProbeServer::reached().await;
        let runtime = runtime_dialing(&server.endpoint(), "kimi-k3");
        // The discriminating seed for AC-5: routing holds this provider
        // down right now, and only a successful call may lift it.
        runtime.record_health(
            "kimi",
            HealthRecord::unavailable(Instant::now(), PROVIDER_UNAVAILABLE_COOLDOWN),
        );
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let session = SessionId::from("sess");

        let result = runtime
            .provider_test(&bus, &session, &ProviderId::from("kimi"))
            .await
            .expect("a completed call is an outcome, not an error");

        assert_eq!(result.provider_id.0, "kimi");
        assert_eq!(result.model, "kimi-k3");
        assert_eq!(
            result.dial_host,
            format!("127.0.0.1:{}", server.port),
            "the dial host is the parser-that-dials' reading, port included"
        );
        let ProviderTestOutcome::Reached {
            input_tokens,
            output_tokens,
            usd_micros,
            ..
        } = result.outcome
        else {
            panic!("a 200 with usage is `reached`: {:?}", result.outcome);
        };
        assert_eq!(
            (input_tokens, output_tokens),
            (REACHED_INPUT, REACHED_OUTPUT)
        );

        // AC-5's daemon half.
        assert_eq!(result.health_after, WireProviderHealth::Healthy);
        assert_eq!(
            runtime.health_snapshot().get("kimi").copied(),
            Some(ProviderHealth::Healthy),
            "the report and the map the router reads are one value"
        );

        // BR-5: one ordinary row, counted apart.
        let report = runtime.ledger.report().expect("report");
        assert_eq!(report.probe_calls, 1);
        assert_eq!(report.total.calls, 1);
        let rows = runtime.ledger.all_records().expect("rows");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].probe, "the row says it was a test");
        assert_eq!(rows[0].provider_id, "kimi");
        assert_eq!(rows[0].model, "kimi-k3");
        assert_eq!(rows[0].session_id, session.0);
        assert_eq!(
            (rows[0].input_tokens, rows[0].output_tokens),
            (REACHED_INPUT, REACHED_OUTPUT)
        );
        assert_eq!(
            usd_micros, rows[0].usd_micros,
            "the reported cost is the ledger's own, so `teton cost` cannot \
             disagree with the report"
        );
        assert!(
            usd_micros.is_some(),
            "`kimi-k3` is in the bundled price table, so this leg is not \
             vacuously comparing two `None`s"
        );

        assert_announced_once(&mut sub, &session, &result);

        // Non-vacuity: the credential really did ride the request, so the
        // 401 test below is testing a request that carried one.
        let requests = server.requests();
        assert_eq!(requests.len(), 1, "one test is one call: {requests:?}");
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("\r\nauthorization:"),
            "the endpoint-bound transport attached the credential: {}",
            requests[0]
        );
    }

    /// **AC-2.** A 401 is `refused`, names the reference, never the key —
    /// including when the vendor's own body echoes it back — writes no
    /// ledger row, and stamps the health a failed turn would.
    #[tokio::test]
    async fn a_refused_test_bills_nothing_and_names_only_the_reference() {
        let server = ProbeServer::refusing("401 Unauthorized").await;
        let runtime = runtime_dialing(&server.endpoint(), "kimi-k3");
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let session = SessionId::from("sess");

        let result = runtime
            .provider_test(&bus, &session, &ProviderId::from("kimi"))
            .await
            .expect("a refusal is an outcome, not an error");

        let ProviderTestOutcome::Refused { status, reason } = &result.outcome else {
            panic!("a 401 is `refused`: {:?}", result.outcome);
        };
        assert_eq!(*status, 401);
        assert!(reason.contains(AUTH_REF), "{reason}");
        assert!(
            !reason.contains(PLANTED_SECRET),
            "ADR-3: the vendor's body echoed the key and the daemon's \
             sentence still does not carry it: {reason}"
        );
        assert!(
            !reason.contains("bad key"),
            "and carries no part of the vendor's prose at all: {reason}"
        );

        // BR-5: a refusal buys nothing, so it is not a call.
        let report = runtime.ledger.report().expect("report");
        assert_eq!(report.total.calls, 0);
        assert_eq!(report.probe_calls, 0);

        // BR-4: the same verdict a failed turn's 401 earns.
        assert_eq!(result.health_after, WireProviderHealth::Unavailable);
        assert_eq!(
            runtime.health_snapshot().get("kimi").copied(),
            Some(ProviderHealth::Unavailable)
        );

        // The event payload is checked for the key too: AC-2 asserts it on
        // the rendered line *and* on every event payload. Read off the
        // envelope the assertion returns, because the subscription it came
        // from is drained by then.
        let announced = format!("{:?}", assert_announced_once(&mut sub, &session, &result));
        assert!(announced.contains(AUTH_REF), "{announced}");
        assert!(!announced.contains(PLANTED_SECRET), "{announced}");
    }

    /// **AC-3, the 404 leg.** The endpoint answered, so the model is what
    /// the sentence names — through the real method, not only the table.
    #[tokio::test]
    async fn a_404_names_the_model_the_config_declares() {
        let server = ProbeServer::refusing("404 Not Found").await;
        let runtime = runtime_dialing(&server.endpoint(), "kimi-k3");
        let bus = Arc::new(EventBus::new());
        let session = SessionId::from("sess");

        let result = runtime
            .provider_test(&bus, &session, &ProviderId::from("kimi"))
            .await
            .expect("a 404 is an outcome");

        let ProviderTestOutcome::UnknownModel { status, reason } = &result.outcome else {
            panic!("{:?}", result.outcome);
        };
        assert_eq!(*status, 404);
        assert!(reason.contains("kimi-k3"), "{reason}");
    }

    /// **AC-3, the 429 leg.** A rate limit is transient, so it is typed as
    /// one and — like a failed turn's 429 — leaves health alone rather than
    /// stranding a provider that is merely busy.
    #[tokio::test]
    async fn a_rate_limited_test_leaves_health_where_it_stood() {
        let server = ProbeServer::refusing("429 Too Many Requests").await;
        let runtime = runtime_dialing(&server.endpoint(), "kimi-k3");
        runtime.record_health("kimi", HealthRecord::degraded());
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let session = SessionId::from("sess");

        let result = runtime
            .provider_test(&bus, &session, &ProviderId::from("kimi"))
            .await
            .expect("a 429 is an outcome");

        assert_eq!(
            result.outcome,
            ProviderTestOutcome::RateLimited {
                retry_after_secs: None
            }
        );
        assert_eq!(
            result.health_after,
            WireProviderHealth::Degraded,
            "a transient failure records nothing, so the standing verdict \
             is what the report says"
        );
        assert_eq!(runtime.ledger.report().expect("report").total.calls, 0);
        assert_announced_once(&mut sub, &session, &result);
    }

    /// **AC-3, the closed-port leg.** Nothing answered, so the outcome is
    /// `unreachable` and names the host that did not.
    #[tokio::test]
    async fn a_closed_port_is_unreachable() {
        // Port 1 on the loopback refuses instantly and resolves nothing —
        // no DNS lookup inside a unit test, and nothing leaves the machine.
        let runtime = runtime_dialing("http://127.0.0.1:1/v1/chat/completions", "kimi-k3");
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let session = SessionId::from("sess");

        let result = runtime
            .provider_test(&bus, &session, &ProviderId::from("kimi"))
            .await
            .expect("a dead port is an outcome");

        let ProviderTestOutcome::Unreachable { reason } = &result.outcome else {
            panic!("{:?}", result.outcome);
        };
        assert!(reason.contains("127.0.0.1:1"), "{reason}");
        assert!(!reason.contains(PLANTED_SECRET), "{reason}");
        assert_eq!(runtime.ledger.report().expect("report").total.calls, 0);
        assert_announced_once(&mut sub, &session, &result);
    }

    /// The probe's request is the minimal one BR-1 describes, asserted on
    /// the bytes rather than on the struct: one user message, no system
    /// prompt, no tools, the floor budget, and **no reasoning field** —
    /// which is what makes `EffortRefused` unreachable (ADR-2).
    #[tokio::test]
    async fn the_probe_sends_one_fixed_message_no_tools_and_no_effort_field() {
        let server = ProbeServer::reached().await;
        let runtime = runtime_dialing(&server.endpoint(), "kimi-k3");
        let bus = Arc::new(EventBus::new());
        let session = SessionId::from("sess");

        runtime
            .provider_test(&bus, &session, &ProviderId::from("kimi"))
            .await
            .expect("reached");

        // The head capture stops at the header terminator, so the body is
        // read off the same buffer's tail.
        let sent = server.requests().remove(0);
        assert!(sent.contains(PROBE_PROMPT), "{sent}");
        assert!(
            sent.contains(&format!("\"max_tokens\":{PROBE_MAX_TOKENS}")),
            "{sent}"
        );
        assert!(!sent.contains("\"tools\""), "{sent}");
        assert!(!sent.contains("\"system\""), "{sent}");
        assert!(
            !sent.contains("reasoning_effort") && !sent.contains("\"thinking\""),
            "a probe states no reasoning field: {sent}"
        );
    }

    // -- verify F1: an answer that is not a completion ------------------

    /// **A 200 that is not a completion stream is `not_a_completion`, not
    /// `reached`** (verify F1).
    ///
    /// The defect this pins is the connection test's worst possible
    /// failure: a *green* answer for an endpoint no turn can use. Both
    /// adapters raise a [`ProviderError`] only at status >= 400, and
    /// `event_stream`'s tail synthesizes a terminal `Completed` even when
    /// the body held no `data:` lines — so before this, an endpoint that
    /// answered `200 {}` produced `Reached { 0 in / 0 out }` **and**
    /// `HealthRecord::healthy()`, which cleared a real downgrade and told
    /// the user their configuration was fine.
    ///
    /// The seeded `Degraded` is what makes the health half discriminating:
    /// a test against an unseeded provider would read `Healthy` either way.
    ///
    /// It is its **own** outcome rather than an `unreachable` wearing a
    /// distinguishing sentence: a host that answered and a host that never
    /// did send the user to two different places (BR-3, LESSON-456).
    #[tokio::test]
    async fn a_200_that_is_not_a_completion_stream_is_not_a_completion() {
        let server = ProbeServer::answering(
            "200 OK",
            "application/json",
            "{\"object\":\"list\",\"data\":[]}".to_owned(),
        )
        .await;
        let runtime = runtime_dialing(&server.endpoint(), "kimi-k3");
        runtime.record_health("kimi", HealthRecord::degraded());
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let session = SessionId::from("sess");

        let result = runtime
            .provider_test(&bus, &session, &ProviderId::from("kimi"))
            .await
            .expect("something answered, so this is an outcome");

        let ProviderTestOutcome::NotACompletion { reason } = &result.outcome else {
            panic!(
                "a 200 with no completion in it is NOT `reached`, and NOT the \
                 `unreachable` a dead host earns: {:?}",
                result.outcome
            );
        };
        assert!(
            reason.contains(&format!("127.0.0.1:{}", server.port)),
            "the sentence names the endpoint that answered wrongly: {reason}"
        );

        // The health half: untouched, in both directions.
        assert_eq!(
            result.health_after,
            WireProviderHealth::Degraded,
            "an answer that is not a completion must not clear a standing \
             downgrade — there is no `FailureClass` for it and no evidence \
             the provider served anything: {result:?}"
        );
        assert_eq!(
            runtime.health_snapshot().get("kimi").copied(),
            Some(ProviderHealth::Degraded),
            "and the map the router reads agrees with the report"
        );

        // Non-vacuity: the request really did go out, which is also why the
        // ledger row below is honest rather than a bug.
        assert_eq!(server.requests().len(), 1, "{:?}", server.requests());
        assert_eq!(
            runtime.ledger.report().expect("report").probe_calls,
            1,
            "the meter bills a status < 400 whose body it polled, and that is \
             left alone deliberately: a request was made and a vendor may \
             charge for it, so suppressing the row would be the ledger lying \
             about egress that happened"
        );
        assert_announced_once(&mut sub, &session, &result);
    }

    /// **A redirect is `not_a_completion` too** (verify F1).
    ///
    /// Redirects are not followed (`Policy::none()`, so a credential header
    /// cannot be carried to an attacker-chosen host), which means a 301 does
    /// not error and does not stream: it is the *same* zero-token
    /// `Completed` a non-SSE 200 produces, arriving by the likeliest real
    /// route — an endpoint pasted without its `/v1` path, or one a vendor
    /// has since moved.
    #[tokio::test]
    async fn a_redirect_is_not_a_completion_rather_than_a_silent_success() {
        let server =
            ProbeServer::answering("301 Moved Permanently", "text/html", String::new()).await;
        let runtime = runtime_dialing(&server.endpoint(), "kimi-k3");
        let bus = Arc::new(EventBus::new());
        let session = SessionId::from("sess");

        let result = runtime
            .provider_test(&bus, &session, &ProviderId::from("kimi"))
            .await
            .expect("a redirect is an outcome");

        assert!(
            matches!(result.outcome, ProviderTestOutcome::NotACompletion { .. }),
            "a 301 is not a completion — the daemon does not follow it, so \
             nothing was ever asked of a chat endpoint: {:?}",
            result.outcome
        );
        assert_eq!(server.requests().len(), 1, "{:?}", server.requests());
    }

    // -- verify F3: the deadline ----------------------------------------

    /// **A vendor that never answers ends as `timed_out`, on the daemon's
    /// own clock** (verify F3).
    ///
    /// The transport carries no timeout by design (a long completion is not
    /// a stalled one), so without [`PROBE_DEADLINE`] this call never
    /// returns: the CLI parks forever, and `timed_out` is an outcome nothing
    /// can produce.
    ///
    /// Driven through [`DaemonRuntime::provider_test_within`] at one second
    /// — the production constant is thirty, which no test may spend. A whole
    /// second rather than something tighter because the outcome reports its
    /// bound in whole seconds, so a sub-second deadline would be reported by
    /// the `max(1)` floor rather than by the value under test, and this
    /// assertion would stop discriminating.
    ///
    /// The elapsed assertion is the non-vacuous half: it fails if the
    /// deadline is not the thing that ended the call.
    #[tokio::test]
    async fn a_vendor_that_never_answers_is_ended_by_the_deadline() {
        let server = ProbeServer::parked().await;
        let runtime = runtime_dialing(&server.endpoint(), "kimi-k3");
        // Seeded so the health claim below is discriminating: an
        // implementation that read the deadline as a persistent failure
        // would move this to `Unavailable`.
        runtime.record_health("kimi", HealthRecord::degraded());
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let session = SessionId::from("sess");

        let started = Instant::now();
        let result = runtime
            .provider_test_within(
                &bus,
                &session,
                &ProviderId::from("kimi"),
                Duration::from_secs(1),
            )
            .await
            .expect("a deadline is an outcome, not an error");
        let waited = started.elapsed();

        let ProviderTestOutcome::TimedOut { after_secs, reason } = &result.outcome else {
            panic!(
                "a hung vendor timed out — which is a different fact from the \
                 `unreachable` a host that never answered at all earns: {:?}",
                result.outcome
            );
        };
        assert_eq!(
            *after_secs, 1,
            "the outcome carries the bound **this call** waited out, typed — \
             which is how a user tells `slow` from `not answering`, and a \
             value reading 30 here would be the compiled-in constant standing \
             in for the deadline that actually ran: {result:?}"
        );
        assert!(
            reason.contains(&format!("127.0.0.1:{}", server.port)),
            "and the sentence names the host that did not answer: {reason}"
        );
        assert!(
            waited < Duration::from_secs(5),
            "the deadline is what ended this call, not a transport error \
             arriving on its own: waited {waited:?}"
        );

        // BR-4: the verdict a turn's own timeout earns, through the turn
        // path's own function — and for a timeout that verdict is *nothing*
        // (`FailureClass::Timeout` is `Retry`, so `health_after_failure`
        // records no downgrade). A provider that is merely slow today is not
        // stranded out of tomorrow's routing.
        assert!(
            health_after_failure(FailureClass::Timeout).is_none(),
            "the premise of the assertion below: a timeout is transient"
        );
        assert_eq!(
            result.health_after,
            WireProviderHealth::Degraded,
            "a probe that hung is the same evidence about this provider that \
             a hung turn is — which is to say, none: the standing verdict is \
             what the report carries: {result:?}"
        );
        assert_announced_once(&mut sub, &session, &result);
    }

    // -- verify F4: two refusals that were classified but never driven ---

    /// **A remote provider with no `model` is refused, and nothing is
    /// dialed** (BUG-155's rule, verify F4).
    ///
    /// The branch existed and was unexercised. What it guards is the failure
    /// where a provider id gets stood in for a model name and the vendor is
    /// asked for a model called `kimi` — so the refusal has to name the
    /// remedy (`--model`) rather than guess.
    #[tokio::test]
    async fn a_remote_provider_with_no_model_is_refused_before_anything_is_dialed() {
        let server = ProbeServer::reached().await;
        let runtime = runtime_dialing(&server.endpoint(), "kimi-k3");
        {
            let mut config = runtime.config.lock().expect("config mutex");
            config.providers[0].model = None;
        }
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let session = SessionId::from("sess");

        let err = runtime
            .provider_test(&bus, &session, &ProviderId::from("kimi"))
            .await
            .expect_err("there is nothing to ask for");

        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(
            err.message.contains("--model"),
            "BUG-155: the refusal hands over the remedy rather than \
             substituting the id for a model name: {}",
            err.message
        );
        assert!(
            server.requests().is_empty(),
            "inspected, not inferred — the server saw {:?}",
            server.requests()
        );
        assert_eq!(runtime.ledger.report().expect("report").total.calls, 0);
        assert!(tested_events(&mut sub).is_empty());
    }

    /// **An endpoint with no dialable host is refused, and the URL is not
    /// echoed back** (verify F4).
    ///
    /// `"not-a-url"` does not parse, so `canonical_host_and_port_of` has no
    /// destination to name — and this branch is reached *before* the
    /// transport is built, which is what keeps the refusal an
    /// `INVALID_PARAMS` about the config rather than a credential error
    /// about a binding that could never have happened.
    ///
    /// The reachable [`ProbeServer`] is the only socket this test owns; its
    /// silence, the empty ledger and the absent event are together the
    /// evidence that no transport was made.
    #[tokio::test]
    async fn an_unparseable_endpoint_is_refused_without_echoing_the_url() {
        let server = ProbeServer::reached().await;
        let runtime = runtime_dialing(&server.endpoint(), "kimi-k3");
        {
            let mut config = runtime.config.lock().expect("config mutex");
            config.providers[0].endpoint = Some("not-a-url".to_owned());
        }
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(16);
        let session = SessionId::from("sess");

        let err = runtime
            .provider_test(&bus, &session, &ProviderId::from("kimi"))
            .await
            .expect_err("there is no host to dial");

        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(
            err.message.contains("kimi"),
            "the refusal names the provider that was asked for: {}",
            err.message
        );
        assert!(
            !err.message.contains("not-a-url"),
            "ADR-3: the endpoint is not echoed back — a URL can carry a \
             credential in its userinfo or its query: {}",
            err.message
        );
        assert!(
            server.requests().is_empty(),
            "no transport was built, so nothing was dialed: {:?}",
            server.requests()
        );
        assert_eq!(runtime.ledger.report().expect("report").total.calls, 0);
        assert!(tested_events(&mut sub).is_empty());
    }
}
