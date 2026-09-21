//! Freeze watchdog.
//!
//! A converted method is a dispatcher loop: the generated code calls
//! [`probe`] once per state transition with the JNI name of the method it is
//! inside and the state it is about to run, after claiming its frame with
//! [`watch`]. A background thread samples every frame of every thread and
//! reports the deepest frame that has **not moved at all** for [`STUCK_AFTER`]:
//!
//! ```text
//! D dex2rs: PARKED 20s in Java_..._NativeEngine_mergeInstalled__... at state 0:
//!            this frame has not advanced a state in 20s, so it is waiting inside
//!            a JNI call (or a monitor) entered from that block.
//! ```
//!
//! The word is `PARKED` and not "stuck" because being parked is the normal state
//! of a converted method that is waiting on the app's own native code -- see
//! "This is a profiler" below. It is what the frame is doing, not a verdict.
//!
//! The line carries the generated method's JNI name and the state number, which
//! is enough to point at the exact block in the generated source -- no `--trace`
//! build, no tombstone, no `kill -3`.
//!
//! # This is a profiler, not a bug detector
//!
//! It is logged at **debug** level on purpose, and it must not be read as "this
//! is broken". A converted method that hands work to the app's own native
//! library is parked inside that JNI call for exactly as long as the native side
//! takes, and nothing inside this runtime can tell a 90-second metadata pass
//! from a call that never returns: both are one frame with a frozen tick. On the
//! app this was written against the build spends ~15 s in `mergeInstalled`,
//! ~90 s in `processLgl` and seconds in `signApk`, so a 5-second threshold reports
//! three legitimate stages of every successful build. That is a stopwatch, which
//! is what it is for -- use it to see *where the time goes*.
//!
//! The one report that *is* a defect is [`crate::spin_report`] (`FREEZE in ...`),
//! logged at error level: it is raised from inside the method that re-entered its
//! own state without leaving, which a JNI call cannot cause.
//!
//! # Why the frame, and not the thread
//!
//! A converted method that calls into Java (the app's own native library, ART,
//! anything) is *parked inside a JNI call* while the callee runs, and the callee
//! can call back into converted code on the same thread. A per-thread counter
//! then keeps moving -- from the nested frames -- while the frame that is
//! actually blocked never transitions again, which is how a 105-second park came
//! out as a mix of `STUCK` and "spinning" lines for the same block. Progress is
//! therefore recorded per frame: nesting is normal (every Compose lambda is a
//! converted method reached through Java), so the innermost frame is a callback
//! as often as it is the caller, and only the frame that stopped is reported.
//!
//! # What it reports and what it does not
//!
//! * A frame that stopped is reported. A frame that keeps moving is not, however
//!   long it dwells in one state -- a build step calling into its own native
//!   library, a 1 s polling loop. The sampler used to report those as `FROZEN`
//!   ("the dispatcher is spinning") with the same `(method, state)` sampled at
//!   every reading, which is indistinguishable from the outside, and on a real
//!   build it named a state whose arm is a bare `return`.
//! * A frame that *stopped* is not necessarily broken either, which is why the
//!   line is a debug line rather than an error: see "This is a profiler" above.
//! * A spin -- an arm that re-enters itself without leaving, which is a compiler
//!   bug -- is named from *inside* the method by its own guard
//!   ([`crate::spin_report`]), which counts consecutive re-entries within one
//!   call and so cannot be fooled by re-entry from the outside.
//!
//! Cost on the hot path is a thread-local (slot, depth) pair plus four relaxed
//! stores per state transition (not per instruction), and one guard push/pop per
//! method call; the sampler wakes twice a second.

use core::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::cell::Cell;
use std::sync::Once;
use std::time::{Duration, Instant};

/// Threads beyond this are not watched (more than enough for a converted app;
/// conversion only runs on Android's own threads).
const MAX_PROBE_SLOTS: usize = 64;

/// Converted frames that may be nested on one thread before the deepest ones
/// stop being tracked.
///
/// Nesting happens whenever a converted method is called back through Java, so
/// it is the normal case rather than an exotic one; 16 is far beyond what a
/// converted app reaches.
const MAX_DEPTH: usize = 16;

/// How long a frame may go without a single state transition before it is
/// reported.
///
/// A converted method that legitimately spends longer than this inside one JNI
/// call -- every build step that hands work to the app's own native library does
/// -- is reported too, which is the reason the report is a log line and nothing
/// else: it says where the time is, not that something is wrong.
const STUCK_AFTER: Duration = Duration::from_secs(5);

/// A frame that is still parked is re-reported at most this often.
const REPORT_EVERY: Duration = Duration::from_secs(5);

/// How long a frame may sit with zero transitions before the watchdog asks the
/// platform for a whole-process thread dump.
///
/// Far past [`STUCK_AFTER`] on purpose. A build step that hands work to the
/// app's own native library parks a frame for tens of seconds and never looks
/// any different from a hang, so only a park lasting *minutes* is worth spending
/// a dump on -- and a dump is read-only, so a request that turns out to be a
/// slow stage costs a burst of logcat and nothing else. A real hang never ends,
/// so it always crosses this.
const DUMP_AFTER: Duration = Duration::from_secs(60);

/// Minimum spacing between dumps, so a thread stuck for ten minutes produces ten
/// dumps rather than six hundred.
const DUMP_EVERY: Duration = Duration::from_secs(60);

/// Sampling period of the watchdog thread.
const SAMPLE_EVERY: Duration = Duration::from_millis(500);

/// `usize::MAX`: this thread has no slot yet. `usize::MAX - 1`: it will never
/// get one (the table is full).
const UNSET: usize = usize::MAX;
const NONE: usize = usize::MAX - 1;

/// One converted frame: what it is running and how far it has got.
///
/// `name_ptr == 0` means "no frame here" -- either the depth is free or the
/// method that used it has returned.
struct Frame {
    /// Pointer into the method name's static byte slice; `0` means idle.
    name_ptr: AtomicUsize,
    name_len: AtomicUsize,
    state: AtomicI64,
    /// Advances on every transition *of this frame*. A frame that is parked
    /// inside a call does not advance it even while the thread, nested frames
    /// and all, keeps working -- which is the whole point of counting here.
    tick: AtomicU64,
}

impl Frame {
    const fn new() -> Self {
        Self {
            name_ptr: AtomicUsize::new(0),
            name_len: AtomicUsize::new(0),
            state: AtomicI64::new(i64::MIN),
            tick: AtomicU64::new(0),
        }
    }
}

/// Every watched thread's frame stack.
static FRAMES: [[Frame; MAX_DEPTH]; MAX_PROBE_SLOTS] =
    [const { [const { Frame::new() }; MAX_DEPTH] }; MAX_PROBE_SLOTS];
static SLOT_COUNT: AtomicUsize = AtomicUsize::new(0);
static WATCHDOG: Once = Once::new();

/// This thread's slot index (fixed once assigned) and how deep it is.
#[derive(Clone, Copy)]
struct Mine {
    slot: usize,
    depth: usize,
}

impl Mine {
    const NEW: Self = Self {
        slot: UNSET,
        depth: 0,
    };
}

thread_local! {
    static MINE: Cell<Mine> = const { Cell::new(Mine::NEW) };
}

/// `(slot, index of the frame this thread is running)`.
#[inline]
fn my_frame() -> (usize, usize) {
    MINE.with(|mine| {
        let mut state = mine.get();
        if state.slot == UNSET {
            let index = SLOT_COUNT.fetch_add(1, Ordering::Relaxed);
            state.slot = if index >= MAX_PROBE_SLOTS {
                NONE
            } else {
                index
            };
            mine.set(state);
        }
        (state.slot, state.depth.saturating_sub(1))
    })
}

/// Keeps this thread's innermost frame on the stack for as long as the converted
/// method that created it is running.
///
/// Every generated entry point creates one of these before its dispatcher and
/// drops it on every exit path (the arm returns, the `break` that unwinds on a
/// pending Java exception, the fallthrough). Nested calls stack, and the drop
/// frees the depth again -- which is what makes a frame's record mean "this
/// method is on the stack right now", and a *stack* of records mean the call
/// chain a park has to be read against.
#[must_use = "the guard must live for the whole method body; dropping it early frees the frame"]
pub struct Watch {
    pushed: bool,
}

/// Mark the calling thread as executing a converted method until the returned
/// guard is dropped. Called once per call of every generated method.
#[inline]
pub fn watch() -> Watch {
    let pushed = MINE.with(|mine| {
        let mut state = mine.get();
        if state.slot == UNSET {
            let index = SLOT_COUNT.fetch_add(1, Ordering::Relaxed);
            state.slot = if index >= MAX_PROBE_SLOTS {
                NONE
            } else {
                index
            };
        }
        if state.depth >= MAX_DEPTH || state.slot >= MAX_PROBE_SLOTS {
            mine.set(state);
            return false;
        }
        // No need to clear the record here: the frame that used this depth
        // cleared it on the way out, and `probe` fills it in.
        state.depth += 1;
        mine.set(state);
        true
    });
    Watch { pushed }
}

impl Drop for Watch {
    #[inline]
    fn drop(&mut self) {
        if !self.pushed {
            return;
        }
        MINE.with(|mine| {
            let mut state = mine.get();
            if state.depth == 0 {
                return;
            }
            state.depth -= 1;
            if state.slot < MAX_PROBE_SLOTS {
                // `name_ptr == 0` is what makes the sampler skip this depth.
                FRAMES[state.slot][state.depth]
                    .name_ptr
                    .store(0, Ordering::Relaxed);
            }
            mine.set(state);
        });
    }
}

/// Start the sampler, once per process.
///
/// `#[cold]` and out of line for a size reason, not a style one: `call_once` plus
/// `thread::Builder::new().name(String)` and `spawn` is a few hundred bytes, and
/// `probe` is inlined at every dispatcher loop -- 3 729 call sites on the test app
/// -- so the whole spawn path was being duplicated into the generated code. It
/// cost ~1 MB of `.text` in a 11 MB library, for code that runs at most once.
#[cold]
#[inline(never)]
fn ensure_watchdog() {
    WATCHDOG.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("dex2rs-watchdog".to_string())
            .spawn(sample_forever);
    });
}

/// Report progress (or the lack of it) from the dispatcher of a generated
/// method. Called once per state transition.
///
/// The hot path is deliberately a handful of relaxed stores: a load of the
/// sampler's flag, one frame lookup, four stores. Everything expensive (the
/// spawn, the sampling, the formatting) lives behind a call.
#[inline]
pub fn probe(name: &'static [u8], state: i64) {
    let (slot, depth) = my_frame();
    if slot == NONE {
        return;
    }
    // A relaxed atomic load once the sampler exists, so the common case never
    // reaches `ensure_watchdog` and its out-of-line body.
    if !WATCHDOG.is_completed() {
        ensure_watchdog();
    }
    let frame = &FRAMES[slot][depth];
    frame
        .name_ptr
        .store(name.as_ptr() as usize, Ordering::Relaxed);
    frame.name_len.store(name.len(), Ordering::Relaxed);
    frame.state.store(state, Ordering::Relaxed);
    frame.tick.fetch_add(1, Ordering::Relaxed);
}

/// The JNI name of the method the calling thread is executing, as recorded by
/// its last [`probe`]. The freeze guard uses it so generated code does not need
/// a second copy of the method name (which would be ~330 KB of `.rodata` on a
/// real app).
pub fn current_method_name() -> String {
    let (slot, depth) = my_frame();
    if slot >= MAX_PROBE_SLOTS {
        return "?".to_string();
    }
    let frame = &FRAMES[slot][depth];
    let ptr = frame.name_ptr.load(Ordering::Relaxed);
    if ptr == 0 {
        return "?".to_string();
    }
    let len = frame.name_len.load(Ordering::Relaxed);
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// What the sampler remembers about one frame between readings.
#[derive(Clone, Copy)]
struct Sampled {
    seen: bool,
    name_ptr: usize,
    state: i64,
    /// When this frame was last seen making progress.
    since: Instant,
    /// When it was last reported.
    reported: Instant,
    /// When a whole-process thread dump was last asked for on its behalf.
    dumped: Instant,
    /// Tick as of the previous reading.
    prev_tick: u64,
}

impl Sampled {
    fn new(at: Instant) -> Self {
        Self {
            seen: false,
            name_ptr: 0,
            state: i64::MIN,
            since: at,
            reported: at,
            dumped: at,
            prev_tick: 0,
        }
    }
}

fn sample_forever() {
    let start = Instant::now();
    let mut seen: Vec<[Sampled; MAX_DEPTH]> =
        vec![[Sampled::new(start); MAX_DEPTH]; MAX_PROBE_SLOTS];

    loop {
        std::thread::sleep(SAMPLE_EVERY);
        let now = Instant::now();
        let live = SLOT_COUNT.load(Ordering::Relaxed).min(MAX_PROBE_SLOTS);
        for index in 0..live {
            // Deepest first: the innermost frame is the call that is blocking,
            // and every frame it is nested inside is blocked *by* it, so the
            // deepest frame that has stopped is the one worth a log line.
            for depth in (0..MAX_DEPTH).rev() {
                let frame = &FRAMES[index][depth];
                let name_ptr = frame.name_ptr.load(Ordering::Relaxed);
                let entry = &mut seen[index][depth];
                if name_ptr == 0 {
                    entry.seen = false;
                    continue;
                }
                let state = frame.state.load(Ordering::Relaxed);
                let tick = frame.tick.load(Ordering::Relaxed);
                if !entry.seen
                    || entry.name_ptr != name_ptr
                    || entry.state != state
                    || tick != entry.prev_tick
                {
                    entry.seen = true;
                    entry.name_ptr = name_ptr;
                    entry.state = state;
                    entry.since = now;
                    entry.prev_tick = tick;
                    continue;
                }
                let stuck_for = now.duration_since(entry.since);
                if stuck_for < STUCK_AFTER || now.duration_since(entry.reported) < REPORT_EVERY {
                    continue;
                }
                entry.reported = now;
                let len = frame.name_len.load(Ordering::Relaxed);
                let bytes = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, len) };
                let name = String::from_utf8_lossy(bytes);
                let secs = stuck_for.as_secs();
                // Debug, not error: a frame parked in a JNI call is how every
                // build step that calls the app's own native library looks, and
                // from here a 90 s native pass is indistinguishable from a call
                // that never returns. `FREEZE` is the error-level report.
                log::debug!(
                    "dex2rs: PARKED {secs}s in {name} at state {state}: this frame has not \
                     advanced a state in {secs}s, so it is waiting inside a JNI call (or a \
                     monitor) entered from that block. Not a fault by itself -- a step that \
                     hands work to the app's own native library waits here for as long as that \
                     work takes, which is tens of seconds on a real build. A park that never \
                     ends is the hang, and at 60s this log asks ART for a thread dump that \
                     names the call it is waiting in."
                );
                if should_dump(stuck_for, now.duration_since(entry.dumped)) {
                    entry.dumped = now;
                    request_thread_dump(&name, secs, state);
                }
                break;
            }
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn getpid() -> i32;
}

/// `SIGQUIT` on POSIX -- what `adb shell kill -3` sends.
#[cfg(unix)]
const SIGQUIT: i32 = 3;

/// Whether a park that has lasted `stuck_for`, last dumped `since_dump` ago,
/// should ask for a dump now. Split out from the sampler so the boundary can be
/// tested without sending a signal at the test process.
fn should_dump(stuck_for: Duration, since_dump: Duration) -> bool {
    stuck_for >= DUMP_AFTER && since_dump >= DUMP_EVERY
}

/// Ask ART for a whole-process Java thread dump.
///
/// ART's `SignalCatcher` is the thing `adb shell kill -3 <pid>` drives, and it
/// answers `SIGQUIT` by writing **every** thread's stack to logcat. Sending it to
/// our own process needs no privilege, no root and no debugger -- which is the
/// point. The one thing a capture of the hang has always needed is for it to
/// happen with nobody watching at the right moment, and this is the only report
/// the runtime can make that can be read afterwards without having grepped live.
///
/// It is a request, not a verdict: a step that hands work to the app's own native
/// code and reports nothing back parks a frame exactly like a hang does, and
/// either way the dump is what says *which* call is parked.
#[cfg(unix)]
fn request_thread_dump(name: &str, secs: u64, state: i64) {
    log::error!(
        "dex2rs: HANG DUMP {secs}s in {name} at state {state}: this frame has made no progress \
         at all, so a whole-process thread dump is being requested. The `art` lines that follow \
         name every thread's Java stack -- find this method and read the frames below it."
    );
    unsafe {
        kill(getpid(), SIGQUIT);
    }
}

/// Non-POSIX hosts have no `SIGQUIT` (the host-JVM probe is not built there).
#[cfg(not(unix))]
fn request_thread_dump(name: &str, secs: u64, state: i64) {
    log::error!(
        "dex2rs: HANG DUMP {secs}s in {name} at state {state}: no thread dump on this platform."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the sampler reads for the calling thread's innermost frame.
    fn sampled() -> (String, i64) {
        let (slot, depth) = my_frame();
        assert!(slot < MAX_PROBE_SLOTS, "the test thread got a slot");
        let frame = &FRAMES[slot][depth];
        let ptr = frame.name_ptr.load(Ordering::Relaxed);
        let state = frame.state.load(Ordering::Relaxed);
        if ptr == 0 {
            return ("?".to_string(), state);
        }
        let len = frame.name_len.load(Ordering::Relaxed);
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
        (String::from_utf8_lossy(bytes).into_owned(), state)
    }

    /// The frame `depth` levels up from the innermost one, as the sampler would
    /// find it by walking the stack.
    fn sampled_at(depth: usize) -> (String, i64) {
        let (slot, _) = my_frame();
        let frame = &FRAMES[slot][depth];
        let ptr = frame.name_ptr.load(Ordering::Relaxed);
        if ptr == 0 {
            return ("?".to_string(), i64::MIN);
        }
        let len = frame.name_len.load(Ordering::Relaxed);
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
        (
            String::from_utf8_lossy(bytes).into_owned(),
            frame.state.load(Ordering::Relaxed),
        )
    }

    fn shown(name: &str, state: i64) -> (String, i64) {
        (name.to_string(), state)
    }

    #[test]
    fn a_returned_method_frees_its_frame() {
        let guard = watch();
        probe(b"Java_a_b_c__", 7);
        assert_eq!(sampled(), shown("Java_a_b_c__", 7));
        drop(guard);
        assert_eq!(
            sampled().0,
            "?",
            "a method that has returned must stop being readable, or the sampler \
             reports the state it ended in for as long as the app runs"
        );
    }

    #[test]
    fn a_nested_call_leaves_the_enclosing_frame_readable() {
        let outer = watch();
        probe(b"Java_outer__", 3);
        {
            let _inner = watch();
            probe(b"Java_inner__", 9);
            assert_eq!(sampled(), shown("Java_inner__", 9));
            // The frame the nested call was made from is still on the stack, and
            // this is exactly what has to stay visible: a method parked in a JNI
            // call whose callee runs converted code must not be renamed by it.
            assert_eq!(sampled_at(0), shown("Java_outer__", 3));
        }
        assert_eq!(
            sampled(),
            shown("Java_outer__", 3),
            "the enclosing frame has to come back to the innermost slot"
        );
        drop(outer);
        assert_eq!(sampled().0, "?");
    }

    #[test]
    fn the_next_call_reclaims_the_depth() {
        drop(watch());
        assert_eq!(sampled().0, "?");
        let guard = watch();
        probe(b"Java_second__", 1);
        assert_eq!(sampled(), shown("Java_second__", 1));
        drop(guard);
        assert_eq!(sampled().0, "?");
    }

    /// Nesting is the normal case, and a frame's record has to belong to that
    /// depth alone: the two frames must not share a tick, or a nested callback
    /// makes the parked frame it was called from look like it is running.
    #[test]
    fn nested_frames_keep_their_own_progress() {
        let outer = watch();
        probe(b"Java_parked__", 40);
        let slot = my_frame().0;
        let outer_tick = FRAMES[slot][0].tick.load(Ordering::Relaxed);

        let _inner = watch();
        for _ in 0..1000 {
            probe(b"Java_callback__", 0);
            probe(b"Java_callback__", 1);
        }
        let inner_slot = slot;
        assert!(
            FRAMES[inner_slot][0].tick.load(Ordering::Relaxed) == outer_tick,
            "the enclosing frame's tick must not move while its callee runs"
        );
        assert!(FRAMES[inner_slot][1].tick.load(Ordering::Relaxed) > 1000);
        assert_eq!(sampled_at(0), shown("Java_parked__", 40));
        drop(outer);
    }

    /// The dump is the only report that survives the run, so its trigger has to
    /// sit well past a stage that merely hands work to native code -- those park
    /// a frame for tens of seconds without looking any different.
    #[test]
    fn a_dump_is_asked_for_only_after_a_long_park() {
        assert!(!should_dump(Duration::from_secs(0), DUMP_EVERY));
        assert!(
            !should_dump(STUCK_AFTER, DUMP_EVERY),
            "a 5 s park is the ordinary native stage this app is full of"
        );
        assert!(!should_dump(
            DUMP_AFTER - Duration::from_secs(1),
            DUMP_EVERY
        ));
        assert!(should_dump(DUMP_AFTER, DUMP_EVERY));
        assert!(
            DUMP_AFTER > STUCK_AFTER * 4,
            "the dump must be far rarer than the log line, or it fires on every stage"
        );
    }

    /// A hung thread produces a dump every `DUMP_EVERY`, not five a second.
    #[test]
    fn dumps_are_spaced_out() {
        assert!(!should_dump(DUMP_AFTER, Duration::from_secs(1)));
        assert!(!should_dump(
            DUMP_AFTER,
            DUMP_EVERY - Duration::from_secs(1)
        ));
        assert!(should_dump(DUMP_AFTER, DUMP_EVERY));
        assert!(should_dump(DUMP_AFTER * 10, DUMP_EVERY));
    }

    /// Both halves of the gate are needed: a long park *and* a gap since the last
    /// dump. Either alone either misses the hang or floods logcat during a slow
    /// stage.
    #[test]
    fn dumps_are_gated_on_the_park_not_just_the_clock() {
        let start = Instant::now();
        // Plenty of time since the last dump, and the park is still too short.
        let long_ago = start + DUMP_EVERY;
        assert!(
            !should_dump(STUCK_AFTER, long_ago.duration_since(start)),
            "a 5 s park must never trigger a dump"
        );
        assert!(!should_dump(
            DUMP_AFTER - Duration::from_secs(1),
            long_ago.duration_since(start)
        ));
        assert!(should_dump(DUMP_AFTER, long_ago.duration_since(start)));

        // A long park immediately after a dump must not ask again -- the dump is
        // still in the buffer, and a second one only hides it.
        let just_dumped = long_ago;
        let soon = just_dumped + Duration::from_secs(1);
        assert!(!should_dump(DUMP_AFTER, soon.duration_since(just_dumped)));
        let later = just_dumped + DUMP_EVERY;
        assert!(should_dump(DUMP_AFTER, later.duration_since(just_dumped)));
    }
}
