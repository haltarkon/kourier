//! The refusal of a helper whose library cannot be used (fix F9): it waits for the app to close
//! its end, but for ONE deadline in all, not one timeout per read.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kourier_tdlib::protocol::{FailureCode, FromHelper, LineReader, decode_from_helper};
use kourier_tdlib::refuse::announce_and_wait;

fn first_frame(app: UnixStream) -> FromHelper {
    app.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let mut lines = LineReader::new(app);
    let line = match lines.next_line() {
        Ok(Some(l)) => l,
        _ => panic!("the helper wrote no frame"),
    };
    decode_from_helper(&line).expect("a frame")
}

#[test]
fn an_app_that_keeps_writing_cannot_keep_a_refusing_helper_alive() {
    use std::io::Write;
    let (app, helper) = UnixStream::pair().expect("pair");
    let mut writer = app.try_clone().expect("clone");
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    // A byte every 20 ms for up to 4 s: every read of the helper gets data before any timeout.
    let feeder = std::thread::spawn(move || {
        for _ in 0..200 {
            if stopped.load(Ordering::Relaxed) || writer.write_all(b"x").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let t0 = Instant::now();
    announce_and_wait(
        helper,
        FailureCode::LibOpenFailed,
        Duration::from_millis(400),
    );
    let took = t0.elapsed();
    stop.store(true, Ordering::Relaxed);
    feeder.join().expect("feeder");
    assert!(
        took >= Duration::from_millis(350),
        "the helper did not wait for the app at all ({took:?})"
    );
    assert!(
        took < Duration::from_millis(2500),
        "the helper outlived its deadline while the app kept writing ({took:?})"
    );
    assert!(matches!(
        first_frame(app),
        FromHelper::Failure {
            code: FailureCode::LibOpenFailed,
            extra: None
        }
    ));
}

#[test]
fn an_app_that_closes_its_end_ends_the_wait_at_once() {
    let (app, helper) = UnixStream::pair().expect("pair");
    let closer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        first_frame_then_close(app);
    });
    let t0 = Instant::now();
    announce_and_wait(
        helper,
        FailureCode::LibSymbolMissing,
        Duration::from_secs(20),
    );
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "the wait did not end when the app closed its end"
    );
    closer.join().expect("closer");
}

fn first_frame_then_close(app: UnixStream) {
    assert!(matches!(
        first_frame(app),
        FromHelper::Failure {
            code: FailureCode::LibSymbolMissing,
            extra: None
        }
    ));
}
