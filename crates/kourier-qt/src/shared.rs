//! State shared by `App` and its sub-objects on the Qt thread: the row source and the model
//! refreshes pending since the last apply.
//!
//! Commands and events merge [`Changes`] here; `App` applies them after the invokable or the
//! event drain that caused them returned (the re-entrancy rule, §8.5), so QML handlers are not
//! re-entered while a model is mid-update.

use std::cell::RefCell;
use std::rc::Rc;

use kourier_viewmodel::{Changes, UiSource};

/// The source plus pending refreshes.
pub struct SharedState {
    /// Where rows come from.
    pub source: Box<dyn UiSource>,
    /// Pending refreshes.
    pub pending: Changes,
}

/// Handle given to every QObject that needs the source. Qt-thread only (`Rc`).
pub type Shared = Rc<RefCell<SharedState>>;

/// Wraps a source.
pub fn shared(source: Box<dyn UiSource>) -> Shared {
    Rc::new(RefCell::new(SharedState {
        source,
        pending: Changes::default(),
    }))
}
