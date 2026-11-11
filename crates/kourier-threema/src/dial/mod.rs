//! THE one module that names the real connectors (TH3): the WebSocket client with the servers'
//! pinned certificates, the HTTPS client and the production servers' data. `kourier-app` is the
//! only crate that calls [`live_parts`] (the source check in `tests/builder.rs` refuses the name and
//! this module's path everywhere else, tests included).
//!
//! A directory (call C6): `pins` holds Threema's certificate pins and the verifier of every TLS
//! connection (host -> pin -> chain); `tls` the two client configurations sharing it and the target
//! check before any DNS lookup; `ws` the WebSocket connector ([`ws::connect_over`] is all of it
//! after the TCP connect); `https` the HTTPS client; `modules` the module paths of these transport
//! crates for the log rule ([`crate::logs`]). Its tests (`tests.rs`, the fixed certificates
//! in `tests/certs.rs`) run in an `unshare -rn` child of the lib test binary and reach nothing but
//! in-memory pipes (`tests/builder.rs` rule 11).
//!
//! NOTHING here dials by being called: [`live_parts`] hands out MAKERS; the actor calls them on
//! its own thread at an account's first connect (TH3 N4), and only a connect dials.

mod https;
mod modules;
mod pins;
mod tls;
mod ws;

#[cfg(test)]
mod tests;

use libthreema::common::config::{Config, ConfigEnvironment};

use crate::parts::{ServerParams, ThreemaParts};

use self::https::LiveHttps;
use self::ws::LiveWs;

pub(crate) use self::modules::TRANSPORT_MODULES;

/// The parts of the application. Lazy: calling this builds no socket and no client.
pub fn live_parts() -> ThreemaParts {
    ThreemaParts::from_makers(
        LiveWs::production,
        LiveHttps::production,
        production_params(),
    )
}

/// The production servers as libthreema's own production configuration names them: Kourier writes
/// no Threema host and no key of its own. The URLs keep libthreema's placeholders (the mediator's
/// device group, the rendezvous path, the blob id); the chat server's keys come in libthreema's
/// order (the primary first).
pub(crate) fn production_params() -> ServerParams {
    let config = Config::from(ConfigEnvironment::Production);
    let multi_device = config.multi_device.as_ref();
    ServerParams {
        mediator_url: multi_device
            .map(|md| md.mediator_server_url.as_str().to_owned())
            .unwrap_or_default(),
        rendezvous_url: multi_device
            .map(|md| md.rendezvous_server_url.as_str().to_owned())
            .unwrap_or_default(),
        directory_url: config.directory_server_url.as_str().to_owned(),
        // The blob MIRROR (multi-device): its URL templates.
        blob_url: multi_device
            .map(|md| md.blob_mirror_server.download_url.as_str().to_owned())
            .unwrap_or_default(),
        blob_done_url: multi_device
            .map(|md| md.blob_mirror_server.done_url.as_str().to_owned())
            .unwrap_or_default(),
        blob_upload_url: multi_device
            .map(|md| md.blob_mirror_server.upload_url.as_str().to_owned())
            .unwrap_or_default(),
        // A key libthreema prints as anything but 64 lowercase hex digits is left out (none is).
        chat_server_keys: config
            .chat_server_public_keys
            .iter()
            .filter_map(|key| hex_32(&key.to_string()))
            .collect(),
    }
}

/// 64 hex digits as 32 bytes.
fn hex_32(text: &str) -> Option<[u8; 32]> {
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let mut bytes = [0_u8; 32];
    let (pairs, _) = digits.as_chunks::<2>();
    for (byte, pair) in bytes.iter_mut().zip(pairs) {
        let pair = std::str::from_utf8(pair).ok()?;
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(bytes)
}
