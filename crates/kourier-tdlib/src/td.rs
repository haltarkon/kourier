//! The engine trait: TDLib's five entry points, implemented by the real library (`dlopen`) and,
//! behind the feature `scripted`, by a scripted TDLib for tests.
//!
//! TDLib aborts the process when two threads wait in its receive call at once. [`split`] makes that
//! impossible by type: it consumes the engine and returns a cloneable [`TdHandle`] for everything
//! but receiving, and exactly ONE [`TdReceiver`], which is neither `Clone` nor `Sync`, so it can be
//! moved into one thread and used there only.

use std::cell::Cell;
use std::ffi::CStr;
use std::marker::PhantomData;
use std::sync::Arc;

/// What TDLib calls for a FATAL log message, right before it aborts. It must not allocate much or
/// block: it writes one fixed frame and returns.
pub type FatalCallback = Box<dyn Fn() + Send + Sync + 'static>;

/// TDLib's JSON interface (`td_json_client.h`): `td_create_client_id`, `td_send`, `td_receive`,
/// `td_execute`, and the log message callback.
pub trait Engine: Send + Sync + 'static {
    /// A new client id; TDLib creates the client at its first request.
    fn create_client(&self) -> i32;
    /// Sends a request (asynchronous; the answer comes through `receive`).
    fn send(&self, client: i32, json: &CStr);
    /// Runs a synchronous request; `None` when TDLib returned nothing.
    fn execute(&self, json: &CStr) -> Option<String>;
    /// The next answer or update of any client, waiting up to `timeout_s`; `None` when there was
    /// none. Never called by two threads at once (see [`split`]).
    fn receive(&self, timeout_s: f64) -> Option<String>;
    /// The callback for FATAL messages (TDLib's `td_set_log_message_callback` at level 0).
    fn set_fatal_callback(&self, cb: FatalCallback);
}

/// Everything of an engine but receiving. Cheap to clone.
pub struct TdHandle<E: Engine> {
    engine: Arc<E>,
}

impl<E: Engine> Clone for TdHandle<E> {
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
        }
    }
}

impl<E: Engine> TdHandle<E> {
    pub fn create_client(&self) -> i32 {
        self.engine.create_client()
    }

    pub fn send(&self, client: i32, json: &CStr) {
        self.engine.send(client, json)
    }

    pub fn execute(&self, json: &CStr) -> Option<String> {
        self.engine.execute(json)
    }

    pub fn set_fatal_callback(&self, cb: FatalCallback) {
        self.engine.set_fatal_callback(cb)
    }
}

/// The one receiving half of an engine: `Send` (it moves into its thread) but neither `Sync` nor
/// `Clone`, so no second thread can receive.
pub struct TdReceiver<E: Engine> {
    engine: Arc<E>,
    _not_sync: PhantomData<Cell<()>>,
}

impl<E: Engine> TdReceiver<E> {
    pub fn receive(&mut self, timeout_s: f64) -> Option<String> {
        self.engine.receive(timeout_s)
    }
}

/// Splits an engine into its handle and its ONE receiver.
pub fn split<E: Engine>(engine: E) -> (TdHandle<E>, TdReceiver<E>) {
    let engine = Arc::new(engine);
    (
        TdHandle {
            engine: Arc::clone(&engine),
        },
        TdReceiver {
            engine,
            _not_sync: PhantomData,
        },
    )
}
