//! The guards of every test that makes a network or a client: a WebSocket connector and an HTTPS
//! client that REFUSE every call and COUNT it, behind the crate's own traits. A test hands them in
//! as the parts ([`parts`], [`counting_parts`]) and runs an actor only in a child under
//! `unshare -rn` (`tests/builder.rs` holds both rules); the child's last check is
//! `dials() == 0 && http_calls() == 0`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use kourier_threema::{
    HttpsClient, HttpsRequest, HttpsResponse, NetError, ServerParams, ThreemaParts, WsConnector,
    WsEvent, WsRequest, WsSender,
};

static WS_CALLS: AtomicUsize = AtomicUsize::new(0);
static HTTP_CALLS: AtomicUsize = AtomicUsize::new(0);

/// WebSockets asked of a [`NoDial`] in this process (0 if the guard did its job).
pub fn dials() -> usize {
    WS_CALLS.load(Ordering::SeqCst)
}

/// HTTPS requests made on a [`NoHttp`] in this process.
pub fn http_calls() -> usize {
    HTTP_CALLS.load(Ordering::SeqCst)
}

/// A connector that refuses every WebSocket and counts. It carries a marker so that a test can see
/// that a client holds it.
pub struct NoDial(#[allow(dead_code)] pub Arc<()>);

#[async_trait]
impl WsConnector for NoDial {
    async fn connect(
        &self,
        _request: WsRequest,
    ) -> Result<(Box<dyn WsSender>, async_channel::Receiver<WsEvent>), NetError> {
        WS_CALLS.fetch_add(1, Ordering::SeqCst);
        Err(NetError::Refused("offline test: websocket"))
    }
}

/// An HTTPS client that refuses every request and counts; it carries the same marker.
pub struct NoHttp(#[allow(dead_code)] pub Arc<()>);

#[async_trait]
impl HttpsClient for NoHttp {
    async fn execute(&self, _request: HttpsRequest) -> Result<HttpsResponse, NetError> {
        HTTP_CALLS.fetch_add(1, Ordering::SeqCst);
        Err(NetError::Refused("offline test: https"))
    }
}

/// The servers' data of the guard parts: hosts under `.invalid` (RFC 6761: they never resolve)
/// and a key of the test's own (0x5a bytes), never a real server's.
pub fn servers() -> ServerParams {
    ServerParams {
        mediator_url: "wss://mediator.invalid/".to_owned(),
        rendezvous_url: "wss://rendezvous.invalid/".to_owned(),
        directory_url: "https://directory.invalid/".to_owned(),
        blob_url: "https://blob.invalid/".to_owned(),
        chat_server_keys: vec![[0x5a; 32]],
    }
}

/// The parts of every test network: guards that refuse and count.
pub fn parts() -> ThreemaParts {
    let marker = Arc::new(());
    let for_http = marker.clone();
    ThreemaParts::from_makers(
        move || NoDial(marker.clone()),
        move || NoHttp(for_http.clone()),
        servers(),
    )
}

/// [`parts`] whose maker also counts, in `made`, how often it was called (the number of clients
/// the network made).
pub fn counting_parts(made: &Arc<AtomicUsize>) -> ThreemaParts {
    let counter = made.clone();
    let marker = Arc::new(());
    let for_http = marker.clone();
    ThreemaParts::from_makers(
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            NoDial(marker.clone())
        },
        move || NoHttp(for_http.clone()),
        servers(),
    )
}
