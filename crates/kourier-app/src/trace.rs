//! Timing marks for the quality targets (architecture §1.3).
//!
//! With `KOURIER_TRACE_STARTUP=1` Kourier writes one line per mark to stderr:
//!
//! ```text
//! kourier-trace: startup.first_frame at_ms=212.4
//! kourier-trace: timeline.open at_ms=1803.9 ms=41.2 rows=50
//! ```
//!
//! `at_ms` is the time since [`init`] (the first statement of `main`); the other fields belong
//! to the mark. `cargo xtask perf` reads these lines. Without the variable nothing is measured
//! and nothing is written.
//!
//! Log safety (§9.1): a mark is a fixed name plus numbers. There is no way to put a text into
//! one, so no message, name or number of a contact can end up in a trace.
//!
//! | Mark | Meaning |
//! |---|---|
//! | `startup.app_started` | `App::start` returned (runtime, keys, `core.db`, core) |
//! | `startup.qt_app` | `QApplication` exists (platform plugin, style, fonts) |
//! | `startup.qml_engine` | the QML engine exists and has its image provider |
//! | `startup.qml_loaded` | `Main.qml` is loaded and the window created (`engine.load` returned) |
//! | `startup.first_frame` | the window's first `frameSwapped` |
//! | `startup.list_populated` | the first frame that shows conversation rows (`rows`) |
//! | `timeline.open` | from `App.openConversation` to the frame that shows its rows (`ms`, `query_ms`, `rows`) |
//! | `window.released` | the hidden window was destroyed and the heap trimmed (§8.2 step 6) |
//! | `window.released_settled` | two seconds later: the heap once the window's objects are really deleted |
//! | `notify.shown` | a message notification was accepted by the server (`ms` since the core's event, `recv_ms` since the envelope where known) |

use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

/// `KOURIER_TRACE_STARTUP=1` turns the marks on.
pub const TRACE_ENV: &str = "KOURIER_TRACE_STARTUP";

/// What every trace line starts with.
pub const LINE_PREFIX: &str = "kourier-trace: ";

struct State {
    start: Instant,
    enabled: bool,
}

static STATE: OnceLock<State> = OnceLock::new();

/// Whether a value of [`TRACE_ENV`] turns tracing on.
fn wants(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1" | "on" | "yes" | "true"))
}

/// Starts the trace clock and reads [`TRACE_ENV`]. `main` calls it first; a later call
/// changes nothing.
pub fn init() {
    let _ = STATE.set(State {
        start: Instant::now(),
        enabled: wants(std::env::var(TRACE_ENV).ok().as_deref()),
    });
}

/// Whether marks are written. `false` before [`init`].
pub fn enabled() -> bool {
    STATE.get().is_some_and(|s| s.enabled)
}

/// Milliseconds since [`init`] (0 before it).
pub fn since_start_ms() -> f64 {
    STATE
        .get()
        .map_or(0.0, |s| s.start.elapsed().as_secs_f64() * 1000.0)
}

/// Whether `name` may name a mark or a field: lower-case letters, digits, `.` and `_`.
pub fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_')
}

/// The line of one mark (without the line break). Fields with a name that is not
/// [`is_name`] or a value that is not finite are left out.
pub fn format_line(name: &str, at_ms: f64, fields: &[(&str, f64)]) -> String {
    let mut line = format!("{LINE_PREFIX}{name} at_ms={at_ms:.1}");
    for (key, value) in fields {
        if !is_name(key) || !value.is_finite() {
            continue;
        }
        // Counts print as integers, times with one decimal.
        if value.fract() == 0.0 && value.abs() < 1e15 {
            let _ = write!(line, " {key}={value:.0}");
        } else {
            let _ = write!(line, " {key}={value:.3}");
        }
    }
    line
}

/// Writes a mark when tracing is on. `name` must be [`is_name`] (anything else is dropped).
pub fn mark(name: &str, fields: &[(&str, f64)]) {
    if !enabled() || !is_name(name) {
        return;
    }
    eprintln!("{}", format_line(name, since_start_ms(), fields));
}

/// A running measurement: [`Span::end`] writes a mark with `ms` since [`Span::begin`].
#[derive(Clone, Copy, Debug)]
pub struct Span {
    started: Instant,
}

impl Span {
    /// Starts measuring; `None` when tracing is off (nothing to pay for then).
    pub fn begin() -> Option<Self> {
        enabled().then(|| Self {
            started: Instant::now(),
        })
    }

    /// Milliseconds since the span began.
    pub fn elapsed_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }

    /// Writes the mark `name` with `ms` and `fields`.
    pub fn end(self, name: &str, fields: &[(&str, f64)]) {
        let mut all = Vec::with_capacity(fields.len() + 1);
        all.push(("ms", self.elapsed_ms()));
        all.extend_from_slice(fields);
        mark(name, &all);
    }
}

/// CPU time used so far, in ms: by the whole process and by the calling thread. A mark that
/// carries both (`cpu_ms`, `thread_cpu_ms`) says whether the time before it was spent
/// computing or waiting.
pub fn cpu_ms() -> (f64, f64) {
    fn of(who: libc::c_int) -> f64 {
        // SAFETY: getrusage fills the struct it is given; a zeroed `rusage` is a valid value.
        let usage = unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            if libc::getrusage(who, &mut usage) != 0 {
                return 0.0;
            }
            usage
        };
        let ms = |t: libc::timeval| {
            #[allow(clippy::cast_precision_loss)]
            let ms = t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
            ms
        };
        ms(usage.ru_utime) + ms(usage.ru_stime)
    }
    (of(libc::RUSAGE_SELF), of(libc::RUSAGE_THREAD))
}

/// The C heap right now, in MB: bytes in use (`malloc` and its large `mmap` blocks) and bytes
/// that are free but still held by the allocator (what `malloc_trim` could give back, and what
/// fragmentation keeps). Both are in the process's memory; only the first is needed.
pub fn heap_mb() -> (f64, f64) {
    // SAFETY: mallinfo2 only reads the allocator's statistics.
    let info = unsafe { libc::mallinfo2() };
    #[allow(clippy::cast_precision_loss)]
    let mb = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
    (mb(info.uordblks + info.hblkhd), mb(info.fordblks))
}

/// Writes the mark `name` with the CPU time used so far ([`cpu_ms`]: `cpu_ms`,
/// `thread_cpu_ms`), the heap ([`heap_mb`]: `heap_used_mb`, `heap_free_mb`) and `fields`.
pub fn mark_with_cpu(name: &str, fields: &[(&str, f64)]) {
    if !enabled() {
        return;
    }
    let (process, thread) = cpu_ms();
    let (used, free) = heap_mb();
    let mut all = Vec::with_capacity(fields.len() + 4);
    all.push(("cpu_ms", process));
    all.push(("thread_cpu_ms", thread));
    all.push(("heap_used_mb", used));
    all.push(("heap_free_mb", free));
    all.extend_from_slice(fields);
    mark(name, &all);
}

/// The instants of one "receive → notification" measurement.
#[derive(Clone, Copy, Debug, Default)]
struct NotifyTimes {
    /// An envelope arrived at a backend.
    arrived: Option<Instant>,
    /// The core's notification event reached the desktop task.
    event: Option<Instant>,
}

static NOTIFY: Mutex<NotifyTimes> = Mutex::new(NotifyTimes {
    arrived: None,
    event: None,
});

fn notify_times() -> std::sync::MutexGuard<'static, NotifyTimes> {
    NOTIFY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An envelope arrived at a backend: the start of "receive → notification" (§1.3). Today only
/// the measuring clock of the reference profile calls it.
pub fn envelope_arrived() {
    if enabled() {
        notify_times().arrived = Some(Instant::now());
        mark("recv.arrived", &[]);
    }
}

/// The core asked for a message notification (the first event of a burst counts).
pub fn notify_event() {
    if enabled() {
        notify_times().event.get_or_insert_with(Instant::now);
    }
}

/// The notification server accepted the notification: writes `notify.shown` with `ms` since
/// [`notify_event`] and, when an arrival was noted, `recv_ms` since [`envelope_arrived`].
pub fn notify_shown() {
    if !enabled() {
        return;
    }
    let times = std::mem::take(&mut *notify_times());
    let ms = |at: Instant| at.elapsed().as_secs_f64() * 1000.0;
    let mut fields = Vec::with_capacity(2);
    if let Some(event) = times.event {
        fields.push(("ms", ms(event)));
    }
    if let Some(arrived) = times.arrived {
        fields.push(("recv_ms", ms(arrived)));
    }
    mark("notify.shown", &fields);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_values_turn_tracing_on() {
        for on in ["1", "on", "yes", "true", " 1 "] {
            assert!(wants(Some(on)), "{on}");
        }
        for off in ["", "0", "off", "no", "2", "debug"] {
            assert!(!wants(Some(off)), "{off}");
        }
        assert!(!wants(None));
    }

    #[test]
    fn a_line_is_a_name_and_numbers() {
        assert_eq!(
            format_line("startup.first_frame", 212.44, &[]),
            "kourier-trace: startup.first_frame at_ms=212.4"
        );
        assert_eq!(
            format_line("timeline.open", 1803.94, &[("ms", 41.25), ("rows", 50.0)]),
            "kourier-trace: timeline.open at_ms=1803.9 ms=41.250 rows=50"
        );
    }

    #[test]
    fn fields_that_are_not_names_or_numbers_are_left_out() {
        let line = format_line(
            "x",
            0.0,
            &[
                ("Name", 1.0),
                ("with space", 1.0),
                ("a=b", 1.0),
                ("nan", f64::NAN),
                ("inf", f64::INFINITY),
                ("ok", 2.0),
            ],
        );
        assert_eq!(line, "kourier-trace: x at_ms=0.0 ok=2");
    }

    #[test]
    fn names_are_lower_case_identifiers() {
        assert!(is_name("timeline.open"));
        assert!(is_name("scroll_p95"));
        assert!(!is_name(""));
        assert!(!is_name("Timeline"));
        assert!(!is_name("a b"));
        assert!(!is_name("a\nb"));
        assert!(!is_name(&"a".repeat(65)));
    }

    #[test]
    fn cpu_time_grows_with_work() {
        let (process, thread) = cpu_ms();
        let mut x = 0u64;
        for i in 0..20_000_000u64 {
            x = x.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(x);
        let (process_after, thread_after) = cpu_ms();
        assert!(process_after >= process && thread_after >= thread);
        assert!(process_after > 0.0 && thread_after > 0.0);
    }

    #[test]
    fn the_heap_counts_what_is_allocated() {
        let (used, _) = heap_mb();
        let block = vec![1u8; 64 * 1024 * 1024];
        std::hint::black_box(&block);
        let (with_block, _) = heap_mb();
        assert!(with_block >= used + 60.0, "{used} MB, then {with_block} MB");
        drop(block);
        let (after, free) = heap_mb();
        assert!(
            after < with_block - 60.0,
            "{with_block} MB, then {after} MB"
        );
        assert!(free >= 0.0);
    }

    #[test]
    fn nothing_is_measured_before_init() {
        // (`init` is never called in this test binary: the marks stay off.)
        assert!(!enabled());
        assert!(Span::begin().is_none());
        mark("startup.first_frame", &[]);
        envelope_arrived();
        notify_event();
        notify_shown();
        let times = *notify_times();
        assert!(times.arrived.is_none() && times.event.is_none());
    }
}
