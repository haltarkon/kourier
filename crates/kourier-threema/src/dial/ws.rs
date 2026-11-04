//! [`LiveWs`]: the real WebSocket connector (the mediator, the rendezvous relay): TCP -> TLS with
//! the pinned verifier ([`super::tls`]) -> the WebSocket upgrade (TH3 §2.12).
//!
//! [`connect_over`] is everything after the TCP connect, over any byte stream: the dial tests drive
//! it over `tokio::io::duplex` against an in-process TLS + WebSocket server; only
//! [`LiveWs::connect`] dials, and no test calls it (`tests/builder.rs` rule 11).
//!
//! THE EVENTS: a binary frame -> [`WsEvent::Binary`]; a close frame -> `Closed` with its code
//! (1005 without one: RFC 6455 §7.1.5) and reason (the actor never shows or logs it); a message
//! over the limit of its kind ([`max_message`]: the mediator's [`MAX_MESSAGE`], the rendezvous
//! relay's [`MAX_RENDEZVOUS_MESSAGE`], decision C6) -> the connector closes with 1009 and reports
//! `Closed { 1009 }`; a text
//! frame (the mediator sends none) -> the connector closes with 4010 and reports `Closed { 4010 }`
//! (Desktop closes on a text frame too); an end without a close frame or a read error ->
//! `Closed { 1006 }`. Ping and pong stay inside tungstenite. The queue to the actor is [`event_depth`]
//! deep by kind (S8: 64 for the mediator, 2 for the rendezvous relay). The read pump is spawned on the CURRENT
//! runtime (the actor's own) and ENDS WITH ITS OWNER: the sender holds the pump's `JoinHandle` and
//! aborts it when dropped (TH3 fix S-L1), so a peer gone silent cannot keep a socket the actor gave
//! up (`tcp_retries2`, ~15 minutes per failed attempt).
//!
//! No deadline here: the actor bounds the whole connect (DNS + TCP + TLS + upgrade) with its 20 s
//! on `ctx.clock` and drops this future at the deadline (decision M10).

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use super::pins::PinSet;
use super::tls::{TlsConfigs, net_error, pinned_target};
use crate::parts::{NetError, UntrustedKind, WsConnector, WsEvent, WsKind, WsRequest, WsSender};

/// The largest message and frame taken from the mediator: the D2M limit (TH2 proof 14). One byte
/// more closes 1009.
pub(crate) const MAX_MESSAGE: usize = 65_536;

/// The largest message and frame taken from the rendezvous relay (TH4 decision C6): one rendezvous
/// frame after the nomination may be 100 MiB (`LT/d2d_rendezvous/frame.rs`, its 4-byte length in
/// front), and whether the phone cuts a big `BlobData` over several messages is [U: real phone].
/// THE REAL PEAK of one rendezvous socket (brief §2.4, critic M7, S8, P11) is about SIX of these
/// messages, 600 MiB: [`event_depth`] two in the queue (the depth of the rendezvous kind), the one the
/// read pump holds while the queue is full, and the one the join works on with its two copies, in
/// libthreema's frame buffer (`add_chunks`; the decryption is in place, so it is the same buffer) and
/// the decoded `EdToNd`. The same applies BEFORE the nomination: libthreema refuses a frame over 16 KiB
/// only once the message is buffered, so anyone with the QR (or the relay) can make Kourier hold the
/// first four copies before the emoji. The join's bounds on arrival (H4, S7) hold the rest.
pub(crate) const MAX_RENDEZVOUS_MESSAGE: usize = 100 * 1_048_576 + 4;

/// The message and frame limit of a socket of `kind` (decision C6).
pub(crate) const fn max_message(kind: WsKind) -> usize {
    match kind {
        WsKind::Mediator => MAX_MESSAGE,
        WsKind::Rendezvous => MAX_RENDEZVOUS_MESSAGE,
    }
}

/// The close code of a text frame: the mediator's "protocol error" (`md-d2m.struct.yml`).
pub(crate) const CLOSE_TEXT_FRAME: u16 = 4010;

/// RFC 6455: a message too big to process.
const CLOSE_TOO_BIG: u16 = 1009;

/// RFC 6455: the socket ended without a close frame.
const CLOSE_ABNORMAL: u16 = 1006;

/// RFC 6455: a close frame without a code.
const CLOSE_NO_STATUS: u16 = 1005;

/// How many events of a socket of `kind` wait for the actor before the pump stops reading (S8): the
/// mediator's messages are at most 64 KiB, a rendezvous message up to [`MAX_RENDEZVOUS_MESSAGE`]
/// (100 MiB), so the rendezvous queue is two deep, not 64 (which was 6.4 GB).
pub(crate) const fn event_depth(kind: WsKind) -> usize {
    match kind {
        WsKind::Mediator => 64,
        WsKind::Rendezvous => 2,
    }
}

/// The real WebSocket connector.
pub(crate) struct LiveWs {
    /// `None` when the TLS configuration could not be made (it never fails with ring; then every
    /// connect fails instead of the application).
    tls: Option<Arc<ClientConfig>>,
    pins: PinSet,
}

impl LiveWs {
    /// The connector over `tls` for the hosts `pins` covers.
    pub(crate) fn new(tls: Arc<ClientConfig>, pins: PinSet) -> Self {
        Self {
            tls: Some(tls),
            pins,
        }
    }

    /// Production: Desktop's six pins (`pins::THREEMA_CH_PINS`). Builds no socket.
    pub(crate) fn production() -> Self {
        match TlsConfigs::production() {
            Ok(configs) => Self::new(configs.ws, PinSet::production()),
            Err(error) => {
                tracing::error!(%error, "the WebSocket's TLS configuration");
                Self {
                    tls: None,
                    pins: PinSet::production(),
                }
            }
        }
    }
}

#[async_trait]
impl WsConnector for LiveWs {
    async fn connect(
        &self,
        request: WsRequest,
    ) -> Result<(Box<dyn WsSender>, async_channel::Receiver<WsEvent>), NetError> {
        // `wss://` to a pinned host, BEFORE any DNS lookup (decision M10).
        let (host, port) = pinned_target(&request.url, "wss", &self.pins)?;
        let tls = self
            .tls
            .clone()
            .ok_or(NetError::Failed("websocket: tls config"))?;
        let tcp = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|_| NetError::Failed("websocket: tcp"))?;
        // A failure only costs latency.
        let _ = tcp.set_nodelay(true);
        connect_over(tcp, &request.url, request.kind, tls).await
    }
}

/// TLS over `stream` with `tls` (the verifier checks host -> pin -> chain), then the WebSocket
/// upgrade to `url` with the limits of `kind`; the read pump is spawned on the current runtime.
pub(crate) async fn connect_over<S>(
    stream: S,
    url: &str,
    kind: WsKind,
    tls: Arc<ClientConfig>,
) -> Result<(Box<dyn WsSender>, async_channel::Receiver<WsEvent>), NetError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let uri: http::Uri = url
        .parse()
        .map_err(|_| NetError::Untrusted(UntrustedKind::UnpinnedHost))?;
    let name = uri
        .host()
        .and_then(|host| ServerName::try_from(host.to_owned()).ok())
        .ok_or(NetError::Untrusted(UntrustedKind::UnpinnedHost))?;
    let stream = TlsConnector::from(tls)
        .connect(name, stream)
        .await
        .map_err(|error| net_error("websocket: tls", &error))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(max_message(kind)))
        .max_frame_size(Some(max_message(kind)));
    let (socket, _response) =
        tokio_tungstenite::client_async_with_config(url.to_owned(), stream, Some(config))
            .await
            .map_err(|_| NetError::Failed("websocket: upgrade"))?;
    let (sink, reader) = socket.split();
    let sink = Arc::new(Mutex::new(sink));
    let (events, receiver) = async_channel::bounded(event_depth(kind));
    let pump = tokio::spawn(pump(reader, sink.clone(), events));
    Ok((Box::new(LiveSender { sink, pump }), receiver))
}

type Sink<S> = Arc<Mutex<SplitSink<WebSocketStream<TlsStream<S>>, Message>>>;

/// The sending half of one socket (shared with its pump, which closes on a text frame). Dropping it
/// ends the pump and so frees the socket (S-L1).
struct LiveSender<S> {
    sink: Sink<S>,
    pump: JoinHandle<()>,
}

impl<S> Drop for LiveSender<S> {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

#[async_trait]
impl<S> WsSender for LiveSender<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&self, frame: Vec<u8>) -> Result<(), NetError> {
        self.sink
            .lock()
            .await
            .send(Message::binary(frame))
            .await
            .map_err(|_| NetError::Failed("websocket: send"))
    }

    async fn close(&self, code: u16, reason: String) -> Result<(), NetError> {
        close(&self.sink, code, reason)
            .await
            .map_err(|_| NetError::Failed("websocket: close"))
    }
}

async fn close<S>(sink: &Sink<S>, code: u16, reason: String) -> Result<(), WsError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    sink.lock()
        .await
        .send(Message::Close(Some(CloseFrame {
            code: CloseCode::from(code),
            reason: reason.into(),
        })))
        .await
}

/// Reads the socket into `events` until it ends; the last event is always a `Closed`.
async fn pump<S>(
    mut reader: SplitStream<WebSocketStream<TlsStream<S>>>,
    sink: Sink<S>,
    events: async_channel::Sender<WsEvent>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let end = loop {
        match reader.next().await {
            Some(Ok(Message::Binary(frame))) => {
                if events.send(WsEvent::Binary(frame.to_vec())).await.is_err() {
                    // Nobody reads any more: the socket is closed like a shutdown.
                    let _ = close(&sink, 1000, String::new()).await;
                    return;
                }
            }
            Some(Ok(Message::Text(_))) => {
                let _ = close(&sink, CLOSE_TEXT_FRAME, String::new()).await;
                break closed(CLOSE_TEXT_FRAME, String::new());
            }
            Some(Ok(Message::Close(frame))) => {
                // tungstenite queued the answering close frame: write it out.
                let _ = sink.lock().await.flush().await;
                break match frame {
                    Some(frame) => closed(frame.code.into(), frame.reason.to_string()),
                    None => closed(CLOSE_NO_STATUS, String::new()),
                };
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
            Some(Err(WsError::Capacity(_))) => {
                let _ = close(&sink, CLOSE_TOO_BIG, String::new()).await;
                break closed(CLOSE_TOO_BIG, String::new());
            }
            Some(Err(_)) | None => break closed(CLOSE_ABNORMAL, String::new()),
        }
    };
    let _ = events.send(end).await;
}

fn closed(code: u16, reason: String) -> WsEvent {
    WsEvent::Closed { code, reason }
}
