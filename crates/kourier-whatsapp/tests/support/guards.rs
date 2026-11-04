//! The guards of every test that builds a client (W1 fix F1): a transport factory and an HTTP client
//! that REFUSE every call and COUNT it. whatsapp-rust is built without its default transport and HTTP
//! client features, so its builder cannot be completed without both; a test hands it these two and
//! runs its body in a child under `unshare -rn` (see `tests/builder.rs`). [`parts`] is the
//! `NetParts` every test network is made with (W2 C2: `WhatsAppNetwork::new` requires them).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use kourier_whatsapp::NetParts;
use wacore::net::{
    HttpClient, HttpRequest, HttpResponse, Transport, TransportEvent, TransportFactory,
};

static TRANSPORT_CALLS: AtomicUsize = AtomicUsize::new(0);
static HTTP_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Dials made on a [`NoDial`] in this process (0 if the guard did its job).
pub fn dials() -> usize {
    TRANSPORT_CALLS.load(Ordering::SeqCst)
}

/// HTTP requests made on a [`NoHttp`] in this process.
pub fn http_calls() -> usize {
    HTTP_CALLS.load(Ordering::SeqCst)
}

/// A transport factory that refuses to dial and counts. It carries a marker so that a test can see
/// that the built client holds it.
pub struct NoDial(#[allow(dead_code)] pub Arc<()>);

#[async_trait]
impl TransportFactory for NoDial {
    async fn create_transport(
        &self,
    ) -> Result<(Arc<dyn Transport>, async_channel::Receiver<TransportEvent>), anyhow::Error> {
        TRANSPORT_CALLS.fetch_add(1, Ordering::SeqCst);
        Err(anyhow::anyhow!("offline test: the dial is refused"))
    }
}

/// An HTTP client that refuses every request and counts; it carries the same marker.
pub struct NoHttp(#[allow(dead_code)] pub Arc<()>);

#[async_trait]
impl HttpClient for NoHttp {
    async fn execute(&self, _request: HttpRequest) -> anyhow::Result<HttpResponse> {
        HTTP_CALLS.fetch_add(1, Ordering::SeqCst);
        Err(anyhow::anyhow!("offline test: the request is refused"))
    }
}

/// The parts of every test network: guards that refuse and count. Every client made from them holds
/// the marker, so a test can see (`Arc::strong_count`) that the client has no other transport or
/// HTTP client.
pub fn parts() -> NetParts {
    let marker = Arc::new(());
    let for_http = marker.clone();
    NetParts::from_makers(
        move || NoDial(marker.clone()),
        move || NoHttp(for_http.clone()),
    )
}

/// [`parts`] whose transport maker also counts, in `made`, how often it was called (the number of
/// clients the network made).
pub fn counting_parts(made: &Arc<AtomicUsize>) -> NetParts {
    let counter = made.clone();
    let marker = Arc::new(());
    let for_http = marker.clone();
    NetParts::from_makers(
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            NoDial(marker.clone())
        },
        move || NoHttp(for_http.clone()),
    )
}
