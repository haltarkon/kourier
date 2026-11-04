//! The script layers of the fake (S-mock §9), the small part TH2 needs:
//!
//! - IMPERATIVE: script calls on [`Net`] that act on the world's mediator and then move what it
//!   sent into the connector's channels (`act`, `close_device`, `advance`), and the waits a test
//!   needs while a client runs beside it (`wait_connected`, [`Net::wait`]).
//! - DECLARATIVE: [`Script`], an ordered list of `send` (the fake does something), `expect` (the
//!   fake saw something, waited for on tokio's clock) and `close` (the mediator closes with a
//!   code); `run` names the first step that was not met. It grows with TH5/TH6 (the ported mocha
//!   cases and the kill windows).
//!
//! Waits run on tokio's clock, which the tests PAUSE: no wall time anywhere.

use std::time::Duration;

use super::connector::Net;
use super::d2m::{ConnId, Mediator};

impl Net {
    /// Runs a script call on the mediator (`csp_incoming`, `csp_echo_request`, `promote` ...) and
    /// hands on what it made the fake send.
    pub fn act<T>(&self, call: impl FnOnce(&Mediator) -> T) -> T {
        let out = call(self.mediator());
        self.flush();
        out
    }

    /// The mediator closes the connector's connection of `device` with `code` (any code of D8).
    /// Whether that device had one that finished the handshake.
    pub fn close_device(&self, device: u64, code: u16) -> bool {
        let Some(conn) = self.connected(device) else {
            return false;
        };
        self.act(|m| m.close(conn, code));
        true
    }

    /// Moves the world's virtual clock AND tokio's paused clock by `by`, runs the fake's timers
    /// (idle 4013, VOLATILE slots, short-lived messages) and hands on what they sent. Needs a
    /// paused clock (`#[tokio::test(start_paused = true)]`): tokio panics otherwise.
    pub async fn advance(&self, by: Duration) {
        tokio::time::advance(by).await;
        self.advance_virtual(by);
    }

    /// Moves ONLY the world's virtual clock by `by` and runs every timer of the fake: the
    /// mediator's, the relay's init timeout (`4003`) and the scripted joiners' (TH4 J2), then hands
    /// on what they sent. The form for a test that moves tokio's clock itself (the actor harness).
    pub fn advance_virtual(&self, by: Duration) {
        self.act(|m| m.advance(by));
        self.relay().tick();
        self.flush();
    }

    /// Waits until the connector's connection of `device` finished the D2M handshake, for at most
    /// `within` on tokio's clock; that connection.
    pub async fn wait_connected(&self, device: u64, within: Duration) -> Option<ConnId> {
        let mut found = None;
        self.wait(
            || {
                found = self.connected(device);
                found.is_some()
            },
            within,
        )
        .await;
        found
    }
}

type Act = Box<dyn FnOnce(&Mediator, ConnId) + Send>;
type Check = Box<dyn FnMut(&Mediator, ConnId) -> bool + Send>;

enum Step {
    Send(&'static str, Act),
    Expect(&'static str, Check),
    Close(u16),
}

/// An ordered script of what the fake does and what it must see on ONE connection (S-mock §9
/// layer 2, the minimal form). The labels are the test's own words (never a key or a text): a
/// failure names the step by number and label.
pub struct Script {
    steps: Vec<Step>,
    within: Duration,
}

impl Default for Script {
    fn default() -> Self {
        Self::new()
    }
}

impl Script {
    /// An empty script; each `expect` waits up to 5 s of tokio's (paused) clock.
    pub fn new() -> Self {
        Self {
            steps: Vec::new(),
            within: Duration::from_secs(5),
        }
    }

    /// How long each `expect` waits.
    #[must_use]
    pub fn within(mut self, within: Duration) -> Self {
        self.within = within;
        self
    }

    /// The fake does something on the connection (a script call of the mediator).
    #[must_use]
    pub fn send(
        mut self,
        label: &'static str,
        act: impl FnOnce(&Mediator, ConnId) + Send + 'static,
    ) -> Self {
        self.steps.push(Step::Send(label, Box::new(act)));
        self
    }

    /// The fake must come to see `check` hold (re-checked after every change of the fake).
    #[must_use]
    pub fn expect(
        mut self,
        label: &'static str,
        check: impl FnMut(&Mediator, ConnId) -> bool + Send + 'static,
    ) -> Self {
        self.steps.push(Step::Expect(label, Box::new(check)));
        self
    }

    /// The mediator closes the connection with `code`.
    #[must_use]
    pub fn close(mut self, code: u16) -> Self {
        self.steps.push(Step::Close(code));
        self
    }

    /// Runs the steps in order on `conn` while the client runs beside it (`tokio::join!`); the
    /// first step that was not met, as a sentence.
    pub async fn run(self, net: &Net, conn: ConnId) -> Result<(), String> {
        for (n, step) in self.steps.into_iter().enumerate() {
            let number = n + 1;
            match step {
                Step::Send(_, act) => net.act(|m| act(m, conn)),
                Step::Close(code) => net.act(|m| m.close(conn, code)),
                Step::Expect(label, mut check) => {
                    if !net.wait(|| check(net.mediator(), conn), self.within).await {
                        return Err(format!(
                            "step {number} (expect {label}) was not met within {} s",
                            self.within.as_secs()
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}
