//! `dex2rs-runtime` — the on-device runtime that generated code links against.
//!
//! This is a port of `d2c/project/jni/nc` (`Dex2C.h`, `Dex2C.cpp`,
//! `well_known_classes.cpp`, `ScopedLocalRef.h`, `obfuscate.h`). The generated
//! code emitted by `dex2rs-core::codegen` calls into this crate instead of using
//! JNI macros, so the surface here is deliberately close to the C++ one:
//!
//! | C++                                             | Rust                              |
//! |-------------------------------------------------|-----------------------------------|
//! | `D2C_RESOLVE_CLASS` / `_METHOD` / `_FIELD`       | [`resolve_class`] / [`resolve_method`] / [`resolve_field`] |
//! | `D2C_CHECK_PENDING_EX`                           | [`exception_check`]               |
//! | `D2C_GET_PENDING_EX`                             | [`get_pending_exception`]         |
//! | `D2C_NOT_NULL`                                   | [`throw_exception`] + `is_null`    |
//! | `D2C_CHECK_CAST`                                 | [`check_cast`]                    |
//!
//! | `AY_OBFUSCATE`                                   | [`obfuscate`]                     |
//! | `d2c_filled_new_array`                           | [`set_int_array_region`], ...      |
//! | `d2c_double_to_long`, ...                        | [`double_to_long`], ...            |
//!
//! Every function that can fail returns `true` when a Java exception is pending,
//! which is the signal the generated code turns into a `goto` to its exception
//! handler — the same contract the C++ macros had.

#![allow(non_snake_case, non_camel_case_types)]
// On docs.rs (nightly) the `trace` hooks get a "Available on crate feature
// trace only" marker; on stable the attribute is not a nightly feature and is
// compiled out entirely.
#![cfg_attr(docsrs, feature(doc_cfg))]
// The whole crate is a thin FFI layer over the JNI function table: every call
// into it is `unsafe`, and wrapping each one again would only add noise.
#![allow(unsafe_op_in_unsafe_fn)]

pub use jni_sys;

/// The JNI environment pointer a native method receives (`JNIEnv *env` in C).
pub type Env = *mut jni_sys::JNIEnv;

/// JNI's `jboolean` is a single byte. `jni-sys` models it as Rust's `bool`,
/// which is not a valid FFI type for an entry point's parameters or return
/// value, so exported methods use this ABI-faithful alias instead
/// (`extern "system" fn(..) -> bool` would be undefined behaviour for any
/// value other than 0/1, and the C++ writer's `jboolean` was `unsigned char`).
pub type jboolean = u8;

/// The crate's own sources, so a driver can vendor them into a project.
///
/// `dex2rs` turns this on (`feature = "vendor-table"`) and writes
/// [`vendored::SOURCES`] into each project it scaffolds; a generated project's
/// own copy has the module `cfg`-ed out, because it has nothing to vendor.
#[cfg(feature = "vendor-table")]
pub mod vendored;

/// Select one entry of the versioned JNI function table.
///
/// jni-sys 0.4 exposes the table as a union of per-version views; this runtime
/// requires JNI 1.6, so the `v1_6` view is used everywhere.
#[macro_export]
macro_rules! jni_fn {
    ($env:expr, $name:ident) => {
        ({
            #[allow(unused_unsafe)]
            let f = unsafe { (*(*$env)).v1_6.$name };
            f
        })
    };
}

pub mod invoke;
pub mod jni;
pub mod ops;
pub mod resolve;
pub mod watchdog;

pub use invoke::*;
pub use jni::*;
pub use ops::*;
pub use resolve::*;
pub use watchdog::*;

/// The wrapped JNI environment type (`jni_sys::JNIEnv`).
pub type JNIEnv = jni_sys::JNIEnv;

// ------------------------------------------------------------- obfuscation ----

/// Key used by [`obfuscate`] / [`new_string_utf_obf`]. `obfuscate.h` derived a key
/// from `__LINE__`; a single fixed key with a bit set in every byte is equally
/// valid (it must only be non-zero in all eight positions).
pub const OBFUSCATE_KEY: u64 = 0x9e37_79b9_7f4a_7c15 | 0x0101_0101_0101_0101;

#[inline]
const fn key_byte(i: usize) -> u8 {
    ((OBFUSCATE_KEY >> ((i % 8) * 8)) & 0xFF) as u8
}

/// Compile-time `AY_OBFUSCATE`: XOR every byte of `data` with `key`.
pub const fn obfuscate<const N: usize>(data: &[u8; N], key: u64) -> [u8; N] {
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = data[i] ^ ((key >> ((i % 8) * 8)) & 0xFF) as u8;
        i += 1;
    }
    out
}

/// The per-string keystream `--obfuscate` XORs with, derived from a 32-bit seed
/// the generator picks for each constant.
///
/// XorShift32, the generator `obfstr` uses. A single fixed key (what
/// [`OBFUSCATE_KEY`] is) means one pass of "XOR the whole `.rodata` with the known
/// key" recovers every obfuscated string in the library; a per-string keystream
/// makes that sweep useless, because the seed differs per constant. Reproducible
/// on purpose: the seed comes from the string itself (`codegen::keystream_seed`),
/// so regenerating a project produces byte-identical output.
#[inline]
#[rustfmt::skip]
pub const fn keystream<const N: usize>(seed: u32) -> [u8; N] {
    let mut keys = [0u8; N];
    let mut x = seed;
    let mut i = 0;
    while i < N {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        // Spread the 32-bit round over four bytes so consecutive bytes of a
        // string do not share a keystream pattern.
        let mut b = 0;
        while b < 4 && i + b < N {
            keys[i + b] = (x >> (b * 8)) as u8;
            b += 1;
        }
        i += 4;
    }
    keys
}

/// XOR `data` with `keys` and hand the result to `NewStringUTF`.
///
/// This is where `--obfuscate` is defeated if it is written the obvious way. The
/// generated constant is a compile-time array and (with a fixed key) so is the
/// keystream, so a plain load lets LLVM compute the plaintext and materialise it
/// -- in `.rodata`, or as immediates in the code -- instead of decrypting at run
/// time. On a real app that put 695 of 1976 obfuscated constants back in
/// `libstub.so`, readable with `strings`.
///
/// Two things keep it out of reach, neither of which costs anything at run time:
///
/// * `core::hint::black_box` on the source pointer is an inline `asm!` barrier,
///   so the bytes behind it are not a value LLVM can reason about;
/// * `#[inline(never)]`, so the call site's constant array and seed are not
///   constant-propagated into this function (LLVM will specialise a callee on its
///   arguments otherwise, `noinline` or not).
///
/// `read_unaligned` is used because the generated literal has alignment 1 -- the
/// alternative, what `obfstr` does, is a `#[repr(C, align(8))]` wrapper around
/// every constant and 8-byte reads, which the emitter would have to name and
/// declare per string.
#[inline(never)]
unsafe fn deobfuscate_to_utf<const N: usize>(
    env: Env,
    data: &[u8; N],
    keys: &[u8; N],
) -> jni_sys::jstring {
    let src = core::hint::black_box(data.as_ptr());
    let mut buf = [0u8; N];
    let mut i = 0;
    while i < N {
        buf[i] = core::ptr::read_unaligned(src.add(i)) ^ keys[i];
        i += 1;
    }
    jni_fn!(env, NewStringUTF)(env, buf.as_ptr() as *const core::ffi::c_char)
}

/// Deobfuscate a [`keystream`]-encrypted constant and hand it to `NewStringUTF`.
///
/// This is what `--obfuscate` emits: the generator has already XORed the bytes
/// (so the plaintext never exists in the generated crate) and picked `seed` from
/// the string.
#[inline]
pub unsafe fn new_string_utf_obf_seeded<const N: usize>(
    env: Env,
    data: &[u8; N],
    seed: u32,
) -> jni_sys::jstring {
    deobfuscate_to_utf(env, data, &keystream::<N>(seed))
}

/// Deobfuscate `data` (which must include its NUL terminator, exactly like the
/// C string literal `AY_OBFUSCATE` was given) and hand it to `NewStringUTF`.
///
/// The fixed-key form, kept because crates generated before the per-string
/// keystream still call it (and [`obfuscate`] with it). New output uses
/// [`new_string_utf_obf_seeded`].
#[inline]
pub unsafe fn new_string_utf_obf<const N: usize>(env: Env, data: &[u8; N]) -> jni_sys::jstring {
    let mut keys = [0u8; N];
    let mut i = 0;
    while i < N {
        keys[i] = key_byte(i);
        i += 1;
    }
    deobfuscate_to_utf(env, data, &keys)
}

// -------------------------------------------------------------- numerics -----

/// `d2c_bitcast_to_float` — `bits` is the raw 32-bit pattern held in a register,
/// so it arrives as a signed value (a `const -1073741824` is the pattern
/// `0xC000_0000`) and is truncated back to `u32` here. Taking `i64` keeps the
/// generated literal unambiguous — `-1073741824 as u32` does not compile in Rust
/// because `as` binds tighter than unary minus.
#[inline]
pub fn bitcast_to_float(bits: i64) -> f32 {
    f32::from_bits(bits as u32)
}

/// `d2c_bitcast_to_double`; `i64 -> u64` reinterprets the register's bit pattern.
#[inline]
pub fn bitcast_to_double(bits: i64) -> f64 {
    f64::from_bits(bits as u64)
}

/// `d2c_double_to_long` — ART's `d2l` semantics: NaN → 0, saturate at the ends.
#[inline]
pub fn double_to_long(v: f64) -> i64 {
    if v.is_nan() {
        0
    } else if v > i64::MAX as f64 {
        i64::MAX
    } else if v < i64::MIN as f64 {
        i64::MIN
    } else {
        v as i64
    }
}

/// `d2c_double_to_int`
#[inline]
pub fn double_to_int(v: f64) -> i32 {
    if v.is_nan() {
        0
    } else if v > i32::MAX as f64 {
        i32::MAX
    } else if v < i32::MIN as f64 {
        i32::MIN
    } else {
        v as i32
    }
}

/// `d2c_float_to_long`
#[inline]
pub fn float_to_long(v: f32) -> i64 {
    if v.is_nan() {
        0
    } else if v > i64::MAX as f32 {
        i64::MAX
    } else if v < i64::MIN as f32 {
        i64::MIN
    } else {
        v as i64
    }
}

/// `d2c_float_to_int`
#[inline]
pub fn float_to_int(v: f32) -> i32 {
    if v.is_nan() {
        0
    } else if v > i32::MAX as f32 {
        i32::MAX
    } else if v < i32::MIN as f32 {
        i32::MIN
    } else {
        v as i32
    }
}

// The method a thread is currently executing, so its per-instruction traces can
// be attributed in the log.
#[cfg(feature = "trace")]
thread_local! {
    static CURRENT: core::cell::Cell<Option<&'static [u8]>> = const { core::cell::Cell::new(None) };
}

/// Mark the entry of a generated method (no-op without the `trace` feature).
///
/// Each native call is a fresh JNI frame, so `--trace` builds print one `enter`
/// line per invocation: the tail of a captured logcat is then a readable call
/// chain ending at whatever is looping.
#[cfg(feature = "trace")]
#[cfg_attr(docsrs, doc(cfg(feature = "trace")))]
#[inline]
pub fn enter(method: &'static [u8]) {
    CURRENT.with(|c| c.set(Some(method)));
    log::debug!("-> {}", String::from_utf8_lossy(method));
}

/// The trace hook the original emitted as `LOGD(...)`; a no-op unless the
/// `trace` feature is enabled.
#[cfg(feature = "trace")]
#[cfg_attr(docsrs, doc(cfg(feature = "trace")))]
#[inline]
pub fn trace(msg: &[u8]) {
    let method = CURRENT.with(|c| c.get());
    log::debug!(
        "[{}] {}",
        String::from_utf8_lossy(method.unwrap_or(b"?")),
        String::from_utf8_lossy(msg)
    );
}

/// Mark the entry of a generated method (no-op without the `trace` feature).
#[cfg(not(feature = "trace"))]
#[inline(always)]
pub fn enter(_method: &'static [u8]) {}

/// The trace hook the original emitted as `LOGD(...)`; a no-op unless the
/// `trace` feature is enabled.
#[cfg(not(feature = "trace"))]
#[inline(always)]
pub fn trace(_msg: &[u8]) {}

/// Report a failure from generated start-up code (`JNI_OnLoad`). Goes to the
/// platform log; a no-op sink when no logger is installed.
#[inline]
pub fn log_error(msg: &str) {
    log::error!("{msg}");
}

/// The freeze guard reported that a generated method re-entered one dispatcher
/// state without leaving it.
///
/// The arm that owns `state` finished without setting `__state`, so the
/// dispatch loop will keep running that same arm. Nothing else can be wrong at
/// this point: every arm either ends in a jump or is the last one, which falls
/// into `EX_UnwindBlock`. Seeing this line means the compiler emitted an arm
/// with no way out, and the method and state name it exactly -- no `--trace`
/// build, no tombstone, no `kill -3`.
///
/// The method name comes from the watchdog slot the dispatcher filled in one
/// instruction earlier ([`watchdog::current_method_name`]), so the emitter only
/// has to pass the state.
///
/// Logged at `error` level so it is visible with the default logcat filter
/// (the `--trace` `debug` lines are not).
#[cold]
#[inline(never)]
pub fn spin_report(state: i64) {
    log::error!(
        "dex2rs: FREEZE in {} at state {state}: that block never leaves its state, \
         so the dispatcher re-runs it forever. This is a compiler bug -- report \
         this line together with the app it came from.",
        watchdog::current_method_name()
    );
}

/// Install the Android logger (the `D2C_DEBUG`/`LOGD` equivalent).
#[cfg(target_os = "android")]
pub fn init_logging() {
    let _ = android_logger::init_once(
        android_logger::Config::default()
            .with_tag("dex2rs")
            .with_max_level(log::LevelFilter::Debug),
    );
}

/// No-op off Android.
#[cfg(not(target_os = "android"))]
pub fn init_logging() {}

/// Everything a generated library needs at start-up: install logging and cache
/// the well-known classes (`JNI_OnLoad`'s `cache_well_known_classes`).
pub unsafe fn init(env: Env) -> bool {
    init_logging();
    unsafe { resolve::init(env) }
}
