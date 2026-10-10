//! The unauthenticated chat socket for sealed sends and prekey fetches with an access key
//! (§6.4): opened on first use, replaced 5 minutes after it was opened (Desktop's
//! `SocketManager.preload.ts:801-841` rotation, so one socket does not tie many sealed sends
//! together for the server), and dropped when it closes or a request on it fails below HTTP.
//!
//! A connect that fails is not repeated before a delay (the same Fibonacci delays with jitter
//! as the authenticated socket, §6.4): requests in between fail at once with a transient
//! error, so a queue of sealed sends cannot turn into a stream of connection attempts. A
//! network change or a resume drops the socket and the delay ([`UnauthSocket::reset`]).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::spawn_local;

use crate::connection::backoff::Backoff;
use crate::net::{ChatEvent, Connected, Net, SignalTransport, TransportError};

/// How long one unauthenticated socket is used.
pub(crate) const ROTATE_AFTER: Duration = Duration::from_secs(5 * 60);
/// How long a replaced socket stays open for requests still running on it.
const RETIRE_GRACE: Duration = Duration::from_secs(30);

struct Current<S> {
    socket: Arc<S>,
    opened_at: u64,
    alive: Rc<Cell<bool>>,
}

/// The account's unauthenticated socket (lives on the actor thread).
pub(crate) struct UnauthSocket<T: SignalTransport> {
    current: RefCell<Option<Current<T::Socket>>>,
    connecting: tokio::sync::Mutex<()>,
    /// Replaced sockets in their grace time: closed with the actor, not left to the runtime.
    retired: Rc<RefCell<Vec<Arc<T::Socket>>>>,
    /// Delays between failed connects.
    backoff: RefCell<Backoff>,
    /// No connect attempt before this (after a failed one).
    not_before: Cell<Option<tokio::time::Instant>>,
}

impl<T: SignalTransport> Default for UnauthSocket<T> {
    fn default() -> Self {
        Self {
            current: RefCell::new(None),
            connecting: tokio::sync::Mutex::new(()),
            retired: Rc::new(RefCell::new(Vec::new())),
            backoff: RefCell::new(Backoff::new()),
            not_before: Cell::new(None),
        }
    }
}

impl<T: SignalTransport> UnauthSocket<T> {
    /// The open socket, if it is alive and young enough.
    fn usable(&self, now_ms: u64) -> Option<Arc<T::Socket>> {
        let current = self.current.borrow();
        let current = current.as_ref()?;
        let young = now_ms.saturating_sub(current.opened_at)
            < u64::try_from(ROTATE_AFTER.as_millis()).unwrap_or(u64::MAX);
        (current.alive.get() && young).then(|| current.socket.clone())
    }

    /// The socket to use now, connecting a new one when needed.
    pub(crate) async fn get(
        &self,
        net: &Net<T>,
        now_ms: u64,
    ) -> Result<Arc<T::Socket>, TransportError> {
        if let Some(socket) = self.usable(now_ms) {
            return Ok(socket);
        }
        let _connecting = self.connecting.lock().await;
        if let Some(socket) = self.usable(now_ms) {
            return Ok(socket);
        }
        if self
            .not_before
            .get()
            .is_some_and(|at| tokio::time::Instant::now() < at)
        {
            return Err(TransportError::Unreachable(
                "unauthenticated socket: the last connect failed, the next waits".to_owned(),
            ));
        }
        let Connected { socket, events } = match net.connect_unauth().await {
            Ok(connected) => connected,
            Err(e) => {
                let unreachable =
                    matches!(e, TransportError::Unreachable(_) | TransportError::Timeout);
                let delay = self.backoff.borrow_mut().next(unreachable);
                self.not_before
                    .set(Some(tokio::time::Instant::now() + delay));
                tracing::warn!(
                    error = %e,
                    retry_in_s = delay.as_secs(),
                    "unauthenticated socket failed to connect"
                );
                return Err(e);
            }
        };
        self.backoff.borrow_mut().reset();
        self.not_before.set(None);
        let socket = Arc::new(socket);
        let alive = Rc::new(Cell::new(true));
        {
            // Unauthenticated sockets get no pushes; the stream only tells when it closed.
            let alive = alive.clone();
            let mut events = events;
            spawn_local(async move {
                while let Some(event) = events.recv().await {
                    if matches!(event, ChatEvent::Stopped(_)) {
                        break;
                    }
                }
                alive.set(false);
            });
        }
        let replaced = self.current.borrow_mut().replace(Current {
            socket: socket.clone(),
            opened_at: now_ms,
            alive,
        });
        if let Some(old) = replaced {
            self.retire(net, old);
        }
        tracing::debug!("unauthenticated socket opened");
        Ok(socket)
    }

    /// The network changed or the machine woke up: the open socket may be dead without knowing
    /// it, and a connect that failed a moment ago may work now. The next request opens a new
    /// socket at once; requests still running on the old one get [`RETIRE_GRACE`].
    pub(crate) fn reset(&self, net: &Net<T>) {
        self.backoff.borrow_mut().reset();
        self.not_before.set(None);
        let old = self.current.borrow_mut().take();
        if let Some(old) = old {
            old.alive.set(false);
            self.retire(net, old);
        }
    }

    /// Closes a replaced socket once the requests still running on it had their time.
    fn retire(&self, net: &Net<T>, old: Current<T::Socket>) {
        let (net, retired) = (net.clone(), self.retired.clone());
        retired.borrow_mut().push(old.socket.clone());
        spawn_local(async move {
            tokio::time::sleep(RETIRE_GRACE).await;
            let waiting = {
                let mut retired = retired.borrow_mut();
                let before = retired.len();
                retired.retain(|socket| !Arc::ptr_eq(socket, &old.socket));
                retired.len() < before
            };
            // Not there any more: `close` took it.
            if waiting {
                net.disconnect(&old.socket).await;
            }
        });
    }

    /// Closes the socket and the replaced ones still open (actor shutdown).
    pub(crate) async fn close(&self, net: &Net<T>) {
        let current = self.current.borrow_mut().take();
        if let Some(current) = current {
            net.disconnect(&current.socket).await;
        }
        let retired = std::mem::take(&mut *self.retired.borrow_mut());
        for socket in retired {
            net.disconnect(&socket).await;
        }
    }

    /// Whether a connect attempt would be refused for now (tests).
    #[cfg(test)]
    pub(crate) fn is_backing_off(&self) -> bool {
        self.not_before
            .get()
            .is_some_and(|at| tokio::time::Instant::now() < at)
    }

    /// Forgets the socket after a failure below HTTP; the next request opens a new one.
    pub(crate) fn invalidate(&self, socket: &Arc<T::Socket>) {
        let mut current = self.current.borrow_mut();
        if current
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(&c.socket, socket))
            && let Some(old) = current.take()
        {
            old.alive.set(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::testing::NoTransport;

    /// §6.4: a failed connect of the unauthenticated socket is not repeated at once, and a
    /// network change lifts the wait.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_connect_is_not_repeated_before_its_delay() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let net = Net::new(
                    Arc::new(NoTransport::default()),
                    tokio::runtime::Handle::current(),
                );
                let unauth = UnauthSocket::<NoTransport>::default();
                // The transport of these tests never connects.
                assert_eq!(
                    unauth.get(&net, 0).await.err(),
                    Some(TransportError::Disconnected)
                );
                assert!(unauth.is_backing_off());
                // Asked again within the delay: refused without an attempt.
                assert!(matches!(
                    unauth.get(&net, 0).await,
                    Err(TransportError::Unreachable(_))
                ));
                unauth.reset(&net);
                assert!(!unauth.is_backing_off(), "a network change lifts the wait");
                assert_eq!(
                    unauth.get(&net, 0).await.err(),
                    Some(TransportError::Disconnected),
                    "and the next request connects again"
                );
            })
            .await;
    }
}
