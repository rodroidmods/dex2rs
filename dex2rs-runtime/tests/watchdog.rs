//! The freeze watchdog has to name the frozen method, not just "hang": a
//! converted method that never leaves a dispatcher state produces no crash, no
//! ANR and no tombstone, so the log line is the only evidence there is.
//!
//! These tests drive the real `probe`/sampler pair (no mocks) and are therefore
//! wall-clock tests: the sampler only reports after `STUCK_AFTER` (5 s) plus one
//! sampling period. They run against the host build of the runtime, which is the
//! same code the Android build links.
//!
//! The sampler reports at **debug** level, because a frame parked in a JNI call
//! is what every build step that calls the app's own native library looks like
//! (see the module docs on `watchdog`); the tests therefore raise the capture
//! filter to `Debug` as well.
//!
//! Every test holds the `watch()` guard for as long as the method would be on
//! the stack, because that guard is what makes a slot mean "this thread is
//! inside that method now".
//!
//! Two of them are regressions, and both are things the sampler got wrong on a
//! real device:
//!
//! * a method that has *returned* must not be reported at all (without the guard
//!   it was reported for as long as the process lived, in the state it returned
//!   from -- the one state a dispatcher cannot loop in);
//! * a method that is *running* must not be reported either, however long it
//!   dwells in one state (a build step that calls into the app's own native
//!   library, a 1 s polling loop: the sampler called those `FROZEN` and described
//!   them as a dispatcher spinning in a state its arm cannot leave).

use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

/// Captures everything the `log` facade is given while the tests run.
struct Capture;

static CAPTURE: Capture = Capture;
static LOGGED: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn capture_logs() {
    let _ = log::set_logger(&CAPTURE);
    // The sampler reports at debug level (it is a profiler, not a bug report),
    // so the capture has to admit debug records or the tests would pass
    // vacuously -- `set_max_level` defaults to `Error`.
    log::set_max_level(log::LevelFilter::Debug);
}

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        LOGGED
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .push(record.args().to_string());
    }
    fn flush(&self) {}
}

fn logged() -> Vec<String> {
    LOGGED
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap()
        .clone()
}

/// Poll for a captured line mentioning `needle`, up to `limit`.
fn wait_for_line(needle: &str, limit: Duration) -> Option<String> {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if let Some(line) = logged().into_iter().find(|l| l.contains(needle)) {
            return Some(line);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    None
}

#[test]
fn blocked_dispatcher_is_named_too() {
    capture_logs();

    // One probe, then nothing: the thread is parked inside a JNI call entered
    // from that block, so the tick does not advance either. The guard is still
    // held, because the method is still on the stack.
    std::thread::spawn(|| {
        let _watch = dex2rs_runtime::watch();
        dex2rs_runtime::probe(b"Java_test_block__", 3);
        std::thread::sleep(Duration::from_secs(30));
    });

    let line = wait_for_line("Java_test_block__", Duration::from_secs(12))
        .expect("the watchdog must report a blocked method");
    assert!(line.contains("PARKED"), "{line}");
    assert!(line.contains("state 3"), "{line}");
}

/// The shape this was rewritten for, straight off a device log: a method parks
/// inside a JNI call, and the callee (the app's own native library) keeps calling
/// back into converted code on the same thread. The *thread* is busy -- a
/// per-thread counter read that as a dispatcher "spinning", alternating with
/// "stuck" for the same block over a 105-second park. The frame that is blocked
/// has not moved, and that is the frame that has to be named.
#[test]
fn a_parked_frame_is_reported_while_callbacks_run_under_it() {
    capture_logs();

    std::thread::spawn(|| {
        let outer = dex2rs_runtime::watch();
        dex2rs_runtime::probe(b"Java_test_outer_parked__", 3);
        // The call that never returns, calling back into converted code as it
        // goes: each callback is entered and left again on this thread.
        for _ in 0..1200 {
            {
                let _inner = dex2rs_runtime::watch();
                dex2rs_runtime::probe(b"Java_test_callback__", 0);
                dex2rs_runtime::probe(b"Java_test_callback__", 1);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(outer);
    });

    let line = wait_for_line("Java_test_outer_parked__", Duration::from_secs(12))
        .expect("the parked frame must be named even though the thread is busy");
    assert!(line.contains("PARKED"), "{line}");
    assert!(line.contains("state 3"), "{line}");
    assert!(
        logged().iter().all(|l| !l.contains("Java_test_callback__")),
        "the callbacks are running, not parked: {:?}",
        logged()
            .into_iter()
            .filter(|l| l.contains("Java_test_callback__"))
            .collect::<Vec<_>>()
    );
}

/// A dispatcher that keeps running is not a hang, and the sampler must not call
/// it one however constant the state looks: this is the shape a real build
/// produced (`NativeEngine.processLgl` at state 3 -- a state whose arm is a bare
/// `return`, so it cannot be a dispatcher that fails to leave). A spin is named
/// from inside the method by its own guard instead, which counts consecutive
/// re-entries within one call and cannot be fooled by re-entry from outside.
#[test]
fn a_running_dispatcher_is_not_reported() {
    capture_logs();

    std::thread::spawn(|| {
        let _watch = dex2rs_runtime::watch();
        // One state at every reading, and a transition per millisecond in
        // between: ~500 transitions per sampling period, which is what a build
        // step calling into its own native library looks like from here.
        for _ in 0..12_000 {
            dex2rs_runtime::probe(b"Java_test_busy__", 7);
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    let noisy = wait_for_line("Java_test_busy__", Duration::from_secs(9));
    assert!(
        noisy.is_none(),
        "a dispatcher that is making progress is not stuck: {noisy:?}"
    );
}

/// The other regression: a method that has returned must not be reported at all,
/// in the state it returned from.
#[test]
fn a_method_that_returns_is_not_reported() {
    capture_logs();

    std::thread::spawn(|| {
        for _ in 0..4000 {
            {
                let _watch = dex2rs_runtime::watch();
                dex2rs_runtime::probe(b"Java_test_returns__", 0);
                dex2rs_runtime::probe(b"Java_test_returns__", 40);
            }
            // The app's own code between two calls, which is where the thread
            // actually spends its time -- and where the slot must read idle.
            std::thread::sleep(Duration::from_millis(2));
        }
    });

    let noisy = wait_for_line("Java_test_returns__", Duration::from_secs(9));
    assert!(
        noisy.is_none(),
        "a method that returned must not be reported: {noisy:?}"
    );
}
