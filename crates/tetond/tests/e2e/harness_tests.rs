//! REQ-623 ADR-6 — the mock provider's own fixture self-tests: request matching
//! and the rendezvous hold, driven over raw HTTP against [`MockProvider`] with
//! no daemon.
//!
//! Concurrent children make a provider's arrival order a scheduler accident,
//! so the suites that test them address each child by what its request *says*
//! ([`MockProvider::start_matching`]) and prove concurrency by parking requests
//! until a count of them has arrived ([`Rendezvous`]). Those suites are only as
//! good as these two primitives, so the primitives are tested first — including
//! the failure they exist to produce (conventions: "verify the failure
//! *mechanism* before building a fixture around it").
//!
//! | Claim | Test |
//! |---|---|
//! | `rendezvous(3)` holds two requests and releases all three on the third; a plain reply on the same provider is not held | [`rendezvous_releases_when_n_parked`] |
//! | two concurrent requests each get their matched reply whichever arrives first; a spent entry falls through to the default | [`matched_reply_reaches_matching_request`] |
//! | `rendezvous(2)` with one request parks it — captured, never answered — and dropping the provider ends it | [`rendezvous_of_two_with_one_request_parks`] |
//!
//! Every reply is collected per request on its own channel, never as "the
//! first one released": which parked request writes first is the scheduler's
//! choice (LESSON-540).
//!
//! Mutation record (run 2026-10-05, each mutation applied alone to
//! `harness.rs` and reverted by edit; reds out of these three tests):
//!
//! | Mutation | Red |
//! |---|---|
//! | release on the first arrival (`state.arrived >= 1` in `Rendezvous::park`) | 2 — both hold tests; matching stays green |
//! | release only the arriving request (the `n`th returns without setting `released`) | 2 — both tests that release; the lone-park test stays green |
//! | ignore the matcher (drop `matcher.matches(body)` in `take_first_match`) | 2 — the matching test and the rendezvous test, whose first held request spends the plain entry |
//! | skip the egress capture on the matching path (drop `record_egress` in `serve_matched`) | 3 |

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::harness::{
    assert_no_boundary_bytes, global_capture, Matcher, MockProvider, MockResponse, Rendezvous,
};

/// The bound on a reply that should come. Generous, so a loaded runner is not a
/// failure; finite, so a broken fixture fails instead of hanging the suite.
const REPLY_DEADLINE: Duration = Duration::from_secs(10);

/// How long a held request must stay unanswered to count as parked. Correct
/// code never answers it, so a longer window only costs time — it cannot flake.
const HOLD_WINDOW: Duration = Duration::from_millis(300);

/// What one raw-HTTP request came back with: `(status, body)`, or why it got
/// no reply.
type Reply = Result<(u16, String), String>;

/// A request body shaped like a chat completion carrying `marker` as the user
/// message — the place a child's `task` text sits in a real request.
fn request_body(marker: &str) -> String {
    json!({
        "model": "mock-model",
        "stream": true,
        "messages": [{ "role": "user", "content": marker }],
    })
    .to_string()
}

/// POST `body` to `endpoint` over a raw `HTTP/1.1` connection and read the
/// whole `Connection: close` reply.
fn post(endpoint: &str, body: &str) -> Reply {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or("endpoint is not http://")?;
    let (addr, path) = rest.split_once('/').ok_or("endpoint has no path")?;
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(REPLY_DEADLINE))
        .map_err(|e| format!("read timeout: {e}"))?;
    let request = format!(
        "POST /{path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut raw = String::new();
    stream
        .read_to_string(&mut raw)
        .map_err(|e| format!("read: {e}"))?;
    let (head, reply) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("no reply: connection closed after {} bytes", raw.len()))?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("malformed status line in {head:?}"))?;
    Ok((status, reply.to_string()))
}

/// [`post`] on its own thread, so several requests can be in flight at once;
/// the reply arrives on the returned channel.
fn post_in_background(endpoint: &str, marker: &str) -> Receiver<Reply> {
    let (tx, rx) = mpsc::channel();
    let endpoint = endpoint.to_string();
    let body = request_body(marker);
    thread::spawn(move || {
        // The receiver may be gone (a test that ended with this request
        // parked); the reply then has nowhere to go, which is fine.
        let _ = tx.send(post(&endpoint, &body));
    });
    rx
}

/// The body of the `200` reply `what` got, within [`REPLY_DEADLINE`].
fn reply_body(rx: &Receiver<Reply>, what: &str) -> String {
    match rx.recv_timeout(REPLY_DEADLINE) {
        Ok(Ok((200, body))) => body,
        Ok(Ok((status, body))) => panic!("{what} was answered {status}: {body:?}"),
        Ok(Err(why)) => panic!("{what} got no reply: {why}"),
        Err(e) => panic!("{what} was not answered within {REPLY_DEADLINE:?}: {e:?}"),
    }
}

/// `what` is still unanswered after [`HOLD_WINDOW`].
fn assert_still_parked(rx: &Receiver<Reply>, what: &str) {
    match rx.recv_timeout(HOLD_WINDOW) {
        Err(RecvTimeoutError::Timeout) => {}
        Ok(reply) => panic!("{what} was answered while it should be parked: {reply:?}"),
        Err(RecvTimeoutError::Disconnected) => panic!("{what}'s client thread died"),
    }
}

/// Some body in the suite-wide egress capture contains `marker`.
fn assert_captured(marker: &str) {
    let capture = global_capture().lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        capture
            .iter()
            .any(|body| body.windows(marker.len()).any(|w| w == marker.as_bytes())),
        "the suite-wide egress capture never saw the request carrying {marker:?}"
    );
}

/// Poll `condition` until it holds or [`REPLY_DEADLINE`] passes.
fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + REPLY_DEADLINE;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} after {REPLY_DEADLINE:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// AC-5's primitive: `rendezvous(3)` holds two requests and releases **all
/// three** when the third arrives. The benign path rides along on the same
/// provider: a plain matched reply is served at once while two requests are
/// parked — a hold parks only its own request, and parked requests do not stop
/// the server accepting the one that will release them.
///
/// Mutation record (run 2026-10-05):
/// - releasing on the first arrival (`state.arrived >= 1` in
///   `Rendezvous::park`) — red: "two arrivals must not release a rendezvous of
///   three".
/// - releasing only the arriving request (the `n`th returns without setting
///   `released`) — red: the first two are never answered ("the first held
///   request got no reply: read: …" at the client's 10 s read timeout).
/// - ignoring the matcher — red: the first held request spends the plain
///   entry, so only one parks and `wait_arrived(2)` times out.
/// - skipping `record_egress` on the matching path — red at
///   `assert_captured("tt423-held-one")`: the capture never saw it.
#[test]
fn rendezvous_releases_when_n_parked() {
    let rv = Rendezvous::new(3);
    let provider = MockProvider::start_matching(
        vec![(
            Matcher::body_contains("tt423-plain"),
            MockResponse::ok("tt423-plain-reply"),
        )],
        rv.hold(MockResponse::ok("tt423-released-reply")),
    );
    let endpoint = provider.openai_endpoint();

    let first = post_in_background(&endpoint, "tt423-held-one");
    let second = post_in_background(&endpoint, "tt423-held-two");
    assert!(
        rv.wait_arrived(2, REPLY_DEADLINE),
        "two requests should reach the rendezvous; {} did",
        rv.arrived()
    );
    assert!(
        !rv.is_released(),
        "two arrivals must not release a rendezvous of three"
    );

    // Benign: not held, and served while two others are parked.
    let plain = post_in_background(&endpoint, "tt423-plain");
    assert_eq!(reply_body(&plain, "the plain request"), "tt423-plain-reply");
    assert_eq!(
        rv.arrived(),
        2,
        "the plain request never touched the rendezvous"
    );

    assert_still_parked(&first, "the first held request");
    assert_still_parked(&second, "the second held request");

    let third = post_in_background(&endpoint, "tt423-held-three");
    // Each reply on its own channel: whichever is released first, all three
    // must arrive (LESSON-540).
    for (rx, what) in [
        (&first, "the first held request"),
        (&second, "the second held request"),
        (&third, "the third held request"),
    ] {
        assert_eq!(reply_body(rx, what), "tt423-released-reply");
    }
    assert!(rv.is_released());
    assert_eq!(rv.arrived(), 3);
    assert_eq!(provider.request_count(), 4);

    for marker in [
        "tt423-held-one",
        "tt423-held-two",
        "tt423-held-three",
        "tt423-plain",
    ] {
        assert_captured(marker);
    }
    assert_no_boundary_bytes();
}

/// AC-6's addressing: two requests in flight at once, with different task text,
/// each get **their** matched reply — in both arrival orders. The order is
/// forced rather than left to the scheduler: the second request is sent only
/// once the first is parked on a shared rendezvous, so both are concurrent and
/// the order is the test's. A spent entry is not served twice: a later request
/// quoting the same task falls through to the default.
///
/// Mutation record (run 2026-10-05):
/// - serving the first live entry whatever the body says (dropping
///   `matcher.matches(body)` from `take_first_match`) — red in the beta-first
///   order: `left: "tt423-reply-alpha"`, `right: "tt423-reply-beta"`.
/// - releasing only the arriving request — red: "alpha got no reply".
/// - skipping `record_egress` on the matching path — red at `assert_captured`.
/// - releasing on the first arrival — green, correctly: matching does not
///   depend on the hold, which here only forces the arrival order.
#[test]
fn matched_reply_reaches_matching_request() {
    for order in [["alpha", "beta"], ["beta", "alpha"]] {
        let rv = Rendezvous::new(2);
        let provider = MockProvider::start_matching(
            vec![
                (
                    Matcher::body_contains("tt423-task-alpha"),
                    rv.hold(MockResponse::ok("tt423-reply-alpha")),
                ),
                (
                    Matcher::body_contains("tt423-task-beta"),
                    rv.hold(MockResponse::ok("tt423-reply-beta")),
                ),
            ],
            MockResponse::ok("tt423-reply-default"),
        );
        let endpoint = provider.openai_endpoint();

        let first = post_in_background(&endpoint, &format!("tt423-task-{}", order[0]));
        assert!(
            rv.wait_arrived(1, REPLY_DEADLINE),
            "the {} request should park first",
            order[0]
        );
        let second = post_in_background(&endpoint, &format!("tt423-task-{}", order[1]));

        assert_eq!(
            reply_body(&first, order[0]),
            format!("tt423-reply-{}", order[0]),
            "arrival order {order:?}"
        );
        assert_eq!(
            reply_body(&second, order[1]),
            format!("tt423-reply-{}", order[1]),
            "arrival order {order:?}"
        );

        // The alpha entry is spent; quoting its task again reaches the default.
        let again = post(&endpoint, &request_body("tt423-task-alpha, quoted later"))
            .expect("the follow-up request is answered");
        assert_eq!(again, (200, "tt423-reply-default".to_string()));
        assert_eq!(provider.request_count(), 3);
    }

    for marker in [
        "tt423-task-alpha",
        "tt423-task-beta",
        "tt423-task-alpha, quoted later",
    ] {
        assert_captured(marker);
    }
    assert_no_boundary_bytes();
}

/// The failure mechanism AC-5 is built on, shown happening: `rendezvous(2)`
/// with only one request parks it. The request arrived — it is in the
/// provider's record and the suite-wide egress capture — and is never
/// answered; that silence is how a sequential implementation deadlocks against
/// the stub. Dropping the provider then ends the parked connection (the client
/// sees it closed, well inside its own 10 s read timeout) instead of stranding
/// a thread.
///
/// Mutation record (run 2026-10-05):
/// - releasing on the first arrival (`state.arrived >= 1` in
///   `Rendezvous::park`) — red: "the lone request was answered while it should
///   be parked".
/// - skipping `record_egress` on the matching path — red at
///   `assert_captured("tt423-lone")`: the capture never saw it.
#[test]
fn rendezvous_of_two_with_one_request_parks() {
    let provider = MockProvider::start_matching(
        Vec::new(),
        MockResponse::rendezvous(2, MockResponse::ok("tt423-never-sent")),
    );
    let lone = post_in_background(&provider.openai_endpoint(), "tt423-lone");

    wait_until("the lone request to arrive", || {
        provider.request_count() == 1
    });
    assert_still_parked(&lone, "the lone request");
    assert_still_parked(&lone, "the lone request");
    // A parked request is egress all the same.
    assert_captured("tt423-lone");

    // `Drop` joins the connection threads, so returning at all proves the
    // parked one gave up; the client must then see its connection closed.
    drop(provider);
    match lone.recv_timeout(Duration::from_secs(2)) {
        Ok(Err(_closed)) => {}
        Ok(Ok(reply)) => panic!("a rendezvous of two answered one request: {reply:?}"),
        Err(e) => panic!("the parked connection outlived its provider: {e:?}"),
    }
    assert_no_boundary_bytes();
}
