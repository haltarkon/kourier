//! [`LiveHttps`]: the real HTTPS client (the directory, the blob mirror; TH6a and later use it, TH3's
//! actor does not; decision C15). reqwest over the HTTPS TLS configuration of [`super::tls`] (the
//! pinned verifier, ALPN `h2` and `http/1.1`), `https_only`, NO proxy from the environment (the
//! account's config says "no proxy", critic L2), no redirects, and Kourier's own target check
//! (`https://` to a pinned host) BEFORE reqwest sees the request: nothing is resolved or dialed for
//! a URL the check refuses.

use std::sync::Arc;

use async_trait::async_trait;
use rustls::ClientConfig;

use super::pins::PinSet;
use super::tls::{TlsConfigs, net_error, pinned_target};
use crate::parts::{
    HttpsClient, HttpsMethod, HttpsRequest, HttpsResponse, NetError, UntrustedKind,
};

/// The largest response body taken when a request names no limit (TH3 fix S-L2): 4 MiB.
pub(crate) const DEFAULT_MAX_BODY: usize = 4 * 1024 * 1024;

/// What a body over its limit is told.
const BODY_TOO_LARGE: NetError = NetError::Failed("https: body too large");

/// The real HTTPS client.
pub(crate) struct LiveHttps {
    /// `None` when the client could not be made (then every request fails, not the application).
    client: Option<reqwest::Client>,
    pins: PinSet,
}

impl LiveHttps {
    /// The client over `tls` for the hosts `pins` covers. Builds no socket.
    pub(crate) fn new(tls: &Arc<ClientConfig>, pins: PinSet) -> Self {
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(ClientConfig::clone(tls))
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build();
        if let Err(error) = &client {
            tracing::error!(error = %net_error("https: client", error), "the HTTPS client");
        }
        Self {
            client: client.ok(),
            pins,
        }
    }

    /// Production: Desktop's six pins (`pins::THREEMA_CH_PINS`). Builds no socket.
    pub(crate) fn production() -> Self {
        match TlsConfigs::production() {
            Ok(configs) => Self::new(&configs.https, PinSet::production()),
            Err(error) => {
                tracing::error!(%error, "the HTTPS client's TLS configuration");
                Self {
                    client: None,
                    pins: PinSet::production(),
                }
            }
        }
    }

    /// Whether the client was made (a test: reqwest took the preconfigured rustls config).
    #[cfg(test)]
    pub(crate) fn is_ready(&self) -> bool {
        self.client.is_some()
    }
}

#[async_trait]
impl HttpsClient for LiveHttps {
    async fn execute(&self, request: HttpsRequest) -> Result<HttpsResponse, NetError> {
        // `https://` to a pinned host, before reqwest parses, resolves or dials anything.
        pinned_target(&request.url, "https", &self.pins)?;
        let url = reqwest::Url::parse(&request.url)
            .map_err(|_| NetError::Untrusted(UntrustedKind::UnpinnedHost))?;
        // The same check on the URL reqwest will use (two parsers must agree on the host).
        if url.scheme() != "https" || !url.host_str().is_some_and(|host| self.pins.covers(host)) {
            return Err(NetError::Untrusted(UntrustedKind::UnpinnedHost));
        }
        let client = self
            .client
            .as_ref()
            .ok_or(NetError::Failed("https: client"))?;
        let method = match request.method {
            HttpsMethod::Get => reqwest::Method::GET,
            HttpsMethod::Post => reqwest::Method::POST,
            HttpsMethod::Put => reqwest::Method::PUT,
        };
        let mut builder = client.request(method, url).body(request.body);
        for (name, value) in request.headers {
            builder = builder.header(name, value);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| net_error("https: request", &error))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
            })
            .collect();
        let body = read_body(response, request.max_body.unwrap_or(DEFAULT_MAX_BODY)).await?;
        Ok(HttpsResponse {
            status,
            headers,
            body,
        })
    }
}

/// The response body, chunk by chunk, up to `limit` bytes: a body that goes over it (declared or
/// chunked without end) is [`BODY_TOO_LARGE`] at the first chunk that crosses the line, so a pinned
/// host cannot make the client hold more than `limit` (TH3 fix S-L2).
pub(crate) async fn read_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, NetError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| net_error("https: body", &error))?
    {
        // `body.len() <= limit` always holds here.
        if chunk.len() > limit - body.len() {
            return Err(BODY_TOO_LARGE);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
