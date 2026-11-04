//! THE one module that names the real WebSocket transport and the real HTTPS client (the list
//! `REAL_CLIENT_MODULES` in `tests/builder.rs` holds this file and no other). It makes the
//! application's parts, lazily: nothing is constructed until an actor asks for a client.
//!
//! It builds no client, connects to nothing and runs nothing at load: [`net_parts`] hands the two
//! makers over as they are, and the HTTP client's agent is built inside its maker ([`RealHttp::new`]),
//! which the actor calls when it makes a client. A test reads this file and fails if that changes
//! (W5b decision G13: the file may define the HTTP client, and only this one). `kourier-app` is the
//! only crate that calls [`net_parts`] (the source check refuses the name everywhere else, tests
//! included), and it does so when it registers the network (W2b).
//!
//! THE HTTP SIDE (W5b decision G12). Kourier's own [`RealHttp`] wraps upstream's ureq client over an
//! agent with timeouts (upstream's default agent has none): connect 30 s, the answer's head 120 s,
//! a body read 300 s (the version file, W7's downloads), no proxy (Signal's network code uses none
//! either), no redirect followed (a 3xx from the CDN is an answer, not a place to go). The upload is
//! Kourier's copy of upstream's (exact `Content-Length`, no chunking, a status read as an answer) with
//! a PER-REQUEST time for the body: 60 s plus the length at 16 KiB/s, a cap on the whole body that ureq's
//! send-body timeout enforces (what bounds what: [`body_budget`]; the attempt's own stall watch,
//! `send::upload`, normally ends a stalled upload sooner); the CDN's answer is read through a 64 KiB cap.
//! The plain requests (the version file, the
//! upload's resume check: `execute`) read their answer through a 4 MiB cap (fix F7; upstream's own cap is 2 GiB, the
//! largest file WhatsApp takes), a streaming download keeps upstream's cap (W7 decides its own).

use std::io::Read;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use wacore::net::{HttpClient, HttpRequest, HttpResponse, StreamingHttpResponse, UploadBody};
use wacore::stats::HttpResourceReport;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use crate::NetParts;

/// The parts of the real application: whatsapp-rust's own WebSocket transport (to WhatsApp's chat
/// server) and Kourier's HTTPS client (the version file of web.whatsapp.com, media). Lazy: calling
/// this builds no socket and no agent; the actor calls the makers when it makes a client.
pub fn net_parts() -> NetParts {
    NetParts::from_makers(TokioWebSocketTransportFactory::new, RealHttp::new)
}

/// Opening a connection (TLS included).
const CONNECT: Duration = Duration::from_secs(30);
/// The head of an answer once the request is sent.
const RECV_RESPONSE: Duration = Duration::from_secs(120);
/// The whole body of an answer (the version file; a download, W7).
const RECV_BODY: Duration = Duration::from_secs(300);
/// The fixed part of an upload body's time.
const BODY_BASE: Duration = Duration::from_secs(60);
/// The slowest upload rate a body is given time for, in bytes per second.
const BODY_RATE: u64 = 16 * 1024;
/// The CDN's answer to an upload is `{"url", "direct_path"}`; anything longer is cut.
const ANSWER_CAP: u64 = 64 * 1024;
/// The most a plain request's answer (`execute`: the version file, the resume check of an upload) is read to: both
/// are small, and an answer that does not stop is an error, not a buffer of up to 2 GiB per host (fix F7).
const EXECUTE_CAP: u64 = 4 * 1024 * 1024;
/// Upstream's buffer sizes and idle pool (`whatsapp-rust-ureq-http-client`'s default agent).
const BUFFER_BYTES: usize = 16 * 1024;
const MAX_IDLE: usize = 3;
const MAX_IDLE_PER_HOST: usize = 2;

/// How long an upload body of `len` bytes may take: [`BODY_BASE`] plus `len` at [`BODY_RATE`].
///
/// WHAT BOUNDS WHAT (ureq 3.4.2, `timeout_send_body`; pinned by the two upload tests, so an upgrade that loses it
/// fails them): ureq counts this time from the end of the request's head. Before each write of a buffer (at most
/// [`BUFFER_BYTES`]) it sets the socket's write timeout, one PERIOD, to the time LEFT of the body, and it refuses a
/// write that starts at or after the deadline (`pool::Connection::transmit_output`), so a SLOW body ends at its time
/// plus the write in flight then. That write's period is what was left when it started, and `write_all` writes again
/// under the SAME period after a partial write: a write fails only when one call moves no byte for a whole period.
/// A STALLED body (the peer takes nothing) therefore ends after a few periods, not one: the receiving kernel still
/// takes a few bytes for a while (it compacts its queue, the window probes find room), three periods measured on
/// loopback (2026-10-28). The ATTEMPT is ended sooner by its stall watch (`send::upload`, 60 s without a read of
/// the source); only the POST's thread waits for the write (architecture §7.1, the lingering POST).
fn body_budget(len: u64, base: Duration, rate: u64) -> Duration {
    base + Duration::from_secs_f64(len as f64 / rate.max(1) as f64)
}

/// The agent every request of one client goes through (see the module doc).
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT))
        .timeout_recv_response(Some(RECV_RESPONSE))
        .timeout_recv_body(Some(RECV_BODY))
        .proxy(None)
        .max_redirects(0)
        .input_buffer_size(BUFFER_BYTES)
        .output_buffer_size(BUFFER_BYTES)
        .max_idle_connections(MAX_IDLE)
        .max_idle_connections_per_host(MAX_IDLE_PER_HOST)
        .build()
        .into()
}

/// Kourier's HTTPS client: upstream's ureq client over [`agent`] for the plain requests and the
/// downloads, Kourier's own upload with its per-request body time. Prints nothing of a request.
#[derive(Clone)]
pub struct RealHttp {
    agent: ureq::Agent,
    /// Upstream's client with its own answer cap: the streaming downloads only (W7's).
    inner: UreqHttpClient,
    /// Upstream's client over the same agent with [`EXECUTE_CAP`]: `execute` only (fix F7).
    plain: UreqHttpClient,
    /// The upload body's time: fixed part and rate ([`body_budget`]); a test shortens them.
    base: Duration,
    rate: u64,
}

impl std::fmt::Debug for RealHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RealHttp")
    }
}

impl RealHttp {
    /// The maker [`net_parts`] hands over: builds the agent (no socket until a request).
    #[must_use]
    pub fn new() -> Self {
        let agent = agent();
        Self {
            inner: UreqHttpClient::with_agent(agent.clone()),
            plain: UreqHttpClient::with_agent(agent.clone()).with_max_body_bytes(EXECUTE_CAP),
            agent,
            base: BODY_BASE,
            rate: BODY_RATE,
        }
    }

    /// The same client with a smaller cap for `execute`'s answers (the real-HTTP test in `tests.rs`).
    #[cfg(test)]
    fn with_execute_cap(mut self, cap: u64) -> Self {
        self.plain = UreqHttpClient::with_agent(self.agent.clone()).with_max_body_bytes(cap);
        self
    }

    /// The same client with a shorter upload body time (the real-HTTP test in `tests.rs`).
    #[cfg(test)]
    fn with_body_time(mut self, base: Duration, rate: u64) -> Self {
        self.base = base;
        self.rate = rate;
        self
    }
}

impl Default for RealHttp {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for RealHttp {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        self.plain.execute(request).await
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    fn execute_streaming(&self, request: HttpRequest) -> Result<StreamingHttpResponse> {
        self.inner.execute_streaming(request)
    }

    fn supports_upload_streaming(&self) -> bool {
        true
    }

    /// Upstream's upload (`whatsapp-rust-ureq-http-client` `execute_upload`) with the body's own
    /// time and the answer's cap. Called from a blocking context; the reader is read on this thread.
    fn execute_upload(
        &self,
        request: HttpRequest,
        body: UploadBody,
        content_length: u64,
    ) -> Result<HttpResponse> {
        anyhow::ensure!(request.method == "POST", "an upload is a POST");
        let mut req = self
            .agent
            .post(&request.url)
            .config()
            .http_status_as_error(false)
            .timeout_send_body(Some(body_budget(content_length, self.base, self.rate)))
            .build();
        for (key, value) in &request.headers {
            req = req.header(key, value);
        }
        // An exact length keeps the body length-delimited (WhatsApp's CDN refuses a chunked one).
        req = req.header("content-length", content_length.to_string());
        let response = req.send(ureq::SendBody::from_owned_reader(body))?;
        let status = response.status().as_u16();
        let mut answer = Vec::new();
        // A read that fails part-way keeps the status (upstream's rule for an error page); a 2xx
        // answer cut by the cap fails to parse where it is read, never passes as complete.
        let _ = response
            .into_body()
            .into_reader()
            .take(ANSWER_CAP)
            .read_to_end(&mut answer);
        Ok(HttpResponse::new(status, answer))
    }

    fn resource_report(&self) -> Option<HttpResourceReport> {
        self.inner.resource_report()
    }
}

#[cfg(test)]
mod tests;
