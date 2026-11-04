//! The real TDLib, opened at run time (TG-1 plan §2.2). The ONLY file of Kourier that names
//! TDLib's entry points and the dynamic loader's calls.
//!
//! Only the helper binary opens the library, and a test opens it only inside an `unshare -rn`
//! child (TG-1b); the app process never does.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;

use crate::td::{Engine, FatalCallback};

/// Why the library could not be used. Carries no loader text (it can name paths).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibError {
    /// The file could not be opened as a shared library.
    Open,
    /// The library lacks this entry point.
    Symbol(&'static str),
}

type CreateFn = unsafe extern "C" fn() -> c_int;
type SendFn = unsafe extern "C" fn(c_int, *const c_char);
type ReceiveFn = unsafe extern "C" fn(f64) -> *const c_char;
type ExecuteFn = unsafe extern "C" fn(*const c_char) -> *const c_char;
type LogCallback = extern "C" fn(c_int, *const c_char);
type SetLogCallbackFn = unsafe extern "C" fn(c_int, Option<LogCallback>);

/// The opened library and its five entry points.
pub struct Dlopen {
    // Kept open for the life of the process: never closed (TDLib's threads run its code).
    _handle: *mut c_void,
    create: CreateFn,
    send: SendFn,
    receive: ReceiveFn,
    execute: ExecuteFn,
    set_log_callback: SetLogCallbackFn,
}

// SAFETY: TDLib's JSON interface may be called from any thread (`td_json_client.h`: "thread-safe"),
// except receiving from two threads at once, which `td::split` rules out by type.
unsafe impl Send for Dlopen {}
// SAFETY: as above.
unsafe impl Sync for Dlopen {}

/// The one fatal callback of the process (TDLib keeps one too).
static FATAL: OnceLock<FatalCallback> = OnceLock::new();

extern "C" fn on_log_message(level: c_int, _message: *const c_char) {
    // Level 0 is FATAL: TDLib aborts right after this returns. The message is never read.
    if level == 0
        && let Some(cb) = FATAL.get()
    {
        cb();
    }
}

fn symbol<T: Copy>(handle: *mut c_void, name: &'static str) -> Result<T, LibError> {
    let cname = CString::new(name).map_err(|_| LibError::Symbol(name))?;
    // SAFETY: `handle` is a live handle of dlopen and `cname` a valid C string.
    let p = unsafe { libc::dlsym(handle, cname.as_ptr()) };
    if p.is_null() {
        return Err(LibError::Symbol(name));
    }
    // SAFETY: `T` is the C function pointer type of `name` in `td_json_client.h` /
    // `td_log.h`; a function pointer has the size of a data pointer on Linux.
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

/// Opens the library at `path` (`RTLD_NOW | RTLD_LOCAL`) and finds its entry points.
pub fn open(path: &Path) -> Result<Dlopen, LibError> {
    let cpath = CString::new(path.as_os_str().as_bytes()).map_err(|_| LibError::Open)?;
    // SAFETY: a valid C string; the flags are the loader's own constants.
    let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(LibError::Open);
    }
    Ok(Dlopen {
        _handle: handle,
        create: symbol(handle, "td_create_client_id")?,
        send: symbol(handle, "td_send")?,
        receive: symbol(handle, "td_receive")?,
        execute: symbol(handle, "td_execute")?,
        set_log_callback: symbol(handle, "td_set_log_message_callback")?,
    })
}

/// Copies a string TDLib returned (valid until its next call of the same kind).
///
/// # Safety
/// `p` is null or a NUL-terminated string that stays valid during the call.
unsafe fn copy_out(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller's promise.
    let s = unsafe { CStr::from_ptr(p) };
    Some(s.to_string_lossy().into_owned())
}

impl Engine for Dlopen {
    fn create_client(&self) -> i32 {
        // SAFETY: a function of the opened library with its C signature.
        unsafe { (self.create)() }
    }

    fn send(&self, client: i32, json: &CStr) {
        // SAFETY: as above; `json` is a valid C string TDLib copies.
        unsafe { (self.send)(client, json.as_ptr()) }
    }

    fn execute(&self, json: &CStr) -> Option<String> {
        // SAFETY: as above; the answer is copied before any other call.
        unsafe { copy_out((self.execute)(json.as_ptr())) }
    }

    fn receive(&self, timeout_s: f64) -> Option<String> {
        // SAFETY: as above; one receiving thread (`td::split`) copies the answer at once.
        unsafe { copy_out((self.receive)(timeout_s)) }
    }

    fn set_fatal_callback(&self, cb: FatalCallback) {
        if FATAL.set(cb).is_ok() {
            // SAFETY: as above; the callback is a plain `extern "C"` function.
            unsafe { (self.set_log_callback)(0, Some(on_log_message)) }
        }
    }
}
