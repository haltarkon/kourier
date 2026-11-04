//! The helper's refusal when its library cannot be used (decisions D3 H4; fix F9).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::protocol::{FailureCode, FromHelper};

/// Writes `F <code>` and waits until the app closes its end, but never longer than `within` in
/// all: ONE deadline, not a timeout per read, so an app that keeps writing cannot keep a helper
/// without a library alive. The wait is there only so the app can look at the process first.
pub fn announce_and_wait(mut sock: UnixStream, code: FailureCode, within: Duration) {
    let deadline = Instant::now() + within;
    let mut buf = Vec::new();
    if (FromHelper::Failure { code, extra: None })
        .encode(&mut buf)
        .is_ok()
    {
        let _ = sock.write_all(&buf);
    }
    let mut sink = [0u8; 256];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        // A zero timeout is an error for the socket, and the time is up anyway.
        if left.is_zero() || sock.set_read_timeout(Some(left)).is_err() {
            return;
        }
        if !matches!(sock.read(&mut sink), Ok(n) if n > 0) {
            return;
        }
    }
}
