//! Thin, inline wrappers over the JNI function table.
//!
//! These correspond one-to-one to the macros and inline helpers in `Dex2C.h`
//! (`D2C_NOT_NULL`, `D2C_CHECK_CAST`, `d2c_is_same_object`, ...).
//!
//! jni-sys 0.4 models the function table as a union of versioned views, so every
//! call site goes through the [`jni_fn!`] macro, which selects the JNI 1.6 view
//! (the minimum this runtime requires).

use core::ffi::c_char;

use jni_sys::*;

use crate::Env;

/// Run `f` with a NUL-terminated copy of `bytes` (class/field/method names are
/// passed around as byte strings so generated code cannot get encodings wrong).
#[inline]
pub fn with_cstr<R>(bytes: &[u8], f: impl FnOnce(*const c_char) -> R) -> R {
    if bytes.contains(&0) {
        // Already NUL-terminated (callers never pass embedded NULs).
        return f(bytes.as_ptr() as *const c_char);
    }
    let mut buf = Vec::with_capacity(bytes.len() + 1);
    buf.extend_from_slice(bytes);
    buf.push(0);
    f(buf.as_ptr() as *const c_char)
}

/// `env->NewLocalRef(obj)` — `NULL` in, `NULL` out.
#[inline]
pub unsafe fn new_local_ref(env: Env, obj: jobject) -> jobject {
    if obj.is_null() {
        obj
    } else {
        jni_fn!(env, NewLocalRef)(env, obj)
    }
}

// ----------------------------------------------------------- reference kinds ---

/// The mask over an ART `IndirectRef`'s kind, which lives in its two low bits.
///
/// From ART's `runtime/indirect_reference_table.h`, verbatim:
///
/// ```text
/// // Indirect reference kind, used as the two low bits of IndirectRef.
/// enum IndirectRefKind {
///   kHandleScopeOrInvalid = 0,
///   kLocal = 1,
///   kGlobal = 2,
///   kWeakGlobal = 3,
/// };
/// ```
///
/// This matters here and nowhere else in the runtime: it is what tells `DeleteLocalRef`
/// apart a reference it may free from a value that only looks like one. See
/// [`delete_local_ref`].
const REFERENCE_KIND_MASK: usize = 0x3;

/// The four kinds, named as ART names them.
///
/// A module rather than four constants in this one: the filter that uses them is
/// Android-only while the tests that pin the encoding are not, so on a desktop build
/// some of them are legitimately unreferenced.
#[allow(dead_code)]
mod kinds {
    /// `kHandleScopeOrInvalid`: a pushed handle scope, or a value that is not a reference.
    pub const HANDLE_SCOPE_OR_INVALID: usize = 0;
    /// `kLocal`: what `DeleteLocalRef` accepts.
    pub const LOCAL: usize = 1;
    /// `kGlobal`: what `NewGlobalRef` returns, and what must go to `DeleteGlobalRef`.
    pub const GLOBAL: usize = 2;
    /// `kWeakGlobal`: `NewWeakGlobalRef`.
    pub const WEAK_GLOBAL: usize = 3;
}

/// The kind ART would read out of `obj`: its two low bits.
///
/// Pure pointer arithmetic, so it is safe to run on a value that is not a reference at
/// all -- which is the whole point. On a host JVM this encoding does not apply and
/// nothing calls it; see [`delete_local_ref`] for the gating.
#[inline]
pub fn reference_kind(obj: jobject) -> usize {
    obj as usize & REFERENCE_KIND_MASK
}

/// Whether `obj` is a reference ART would call a *local* one.
#[inline]
pub fn is_local_reference(obj: jobject) -> bool {
    reference_kind(obj) == kinds::LOCAL
}

/// Say once -- loudly -- that a delete was refused, with the value that was refused.
///
/// Once rather than per call: a converted method that does this does it on every
/// invocation, and a log line per call would be its own kind of crash report noise.
#[cfg(target_os = "android")]
fn report_non_local_reference(obj: jobject) {
    use core::sync::atomic::{AtomicBool, Ordering};
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    log::error!(
        "dex2rs: refusing to release {:#x} as a JNI local reference: ART reads it as a {} \
         (kind {}), so `DeleteLocalRef` would abort the process with \"Attempt to delete \
         global reference reference as local JNI reference\". A reference-typed slot held \
         something that is not a local reference -- a small integer constant is the case \
         this was found on -- so there is nothing to free and execution continues",
        obj as usize,
        kind_name(reference_kind(obj)),
        reference_kind(obj),
    );
}

/// How ART names a reference kind, for that log line.
#[cfg(target_os = "android")]
fn kind_name(kind: usize) -> &'static str {
    match kind {
        kinds::HANDLE_SCOPE_OR_INVALID => "handle scope or a value that is not a reference",
        kinds::LOCAL => "local reference",
        kinds::GLOBAL => "global reference",
        kinds::WEAK_GLOBAL => "weak global reference",
        _ => "unknown reference kind",
    }
}

/// `env->DeleteLocalRef(obj)` -- and nothing else, ever.
///
/// # Why this checks what it was handed
///
/// `DeleteLocalRef` is not a no-op on a value it does not like: ART reads the two low
/// bits of the pointer as a reference kind (see `REFERENCE_KIND_MASK` above) and, unless they
/// say `kLocal`, aborts the whole process with
///
/// ```text
/// F local_reference_table.cc:452] Attempt to delete global reference reference as local JNI reference
/// F libc: Fatal signal 6 (SIGABRT)
/// ```
///
/// That is not a hypothetical. Generated code can put a value that is not a reference into
/// a reference-typed slot, and one real case is a small integer constant: `const/4 v6, #2`
/// on one path and an object on another leaves a register whose value is `2`, typed
/// `jobject` by inference, and `2 & 3 == 2` is `kGlobal` -- exactly the message above,
/// from the `ToolchainManager$install$3.invokeSuspend` of this very tool's reference app.
///
/// # What is skipped, and why skipping is the correct answer
///
/// A value whose kind is not `kLocal` is one of two things, and dropping the call is right
/// for both:
///
/// * a **global or weak-global reference** -- there is no local reference to release, and
///   the caller that handed it over never had one;
/// * **not a reference at all** -- a numeric constant or a stale value, where the call had
///   no meaning to begin with.
///
/// Nothing leaks by skipping: a genuine local reference is always `kLocal`, and a local
/// reference table segment is dropped whole when the native frame returns, so the worst
/// case for a refused delete is one entry that lives until the method returns.
///
/// The check is Android-only on purpose. It is ART's encoding, quoted above; on a host JVM
/// (the `jvm_probe` example, the host-side tests) the table encoding is HotSpot's, the
/// values are only ever real references, and a wrong answer here would leak real locals.
#[inline]
pub unsafe fn delete_local_ref(env: Env, obj: jobject) {
    if obj.is_null() {
        return;
    }
    #[cfg(target_os = "android")]
    if !is_local_reference(obj) {
        report_non_local_reference(obj);
        return;
    }
    jni_fn!(env, DeleteLocalRef)(env, obj);
}

/// The kinds [`delete_local_ref`] filters on, as a test can see them.
#[cfg(test)]
mod reference_kind_tests {
    use super::*;

    /// The value a `const/4` can leave in a reference-typed slot, which is the one that
    /// used to abort the VM: `2 & 3` is `kGlobal`.
    #[test]
    fn a_small_integer_is_not_a_local_reference() {
        assert_eq!(reference_kind(0 as jobject), kinds::HANDLE_SCOPE_OR_INVALID);
        assert_eq!(reference_kind(1 as jobject), kinds::LOCAL);
        assert_eq!(reference_kind(2 as jobject), kinds::GLOBAL);
        assert_eq!(reference_kind(3 as jobject), kinds::WEAK_GLOBAL);
        assert!(!is_local_reference(2 as jobject));
        assert!(is_local_reference(1 as jobject));
    }

    /// A real table entry is a pointer far above the low bits, and only its kind decides.
    #[test]
    fn a_table_entry_is_judged_by_its_low_bits_alone() {
        let local = (0x7f0000001000usize | kinds::LOCAL) as jobject;
        let global = (0x7f0000001000usize | kinds::GLOBAL) as jobject;
        assert!(is_local_reference(local));
        assert!(!is_local_reference(global));
        assert_eq!(reference_kind(local), kinds::LOCAL);
    }
}

/// `D2C_CHECK_PENDING_EX`
#[inline]
pub unsafe fn exception_check(env: Env) -> bool {
    jni_fn!(env, ExceptionCheck)(env)
}

/// `D2C_GET_PENDING_EX` — take the pending exception (and clear it) so a landing
/// pad can dispatch on its type.
#[inline]
pub unsafe fn get_pending_exception(env: Env) -> jthrowable {
    let exception = jni_fn!(env, ExceptionOccurred)(env);
    jni_fn!(env, ExceptionClear)(env);
    exception
}

/// `env->Throw(throwable)`
#[inline]
pub unsafe fn throw(env: Env, throwable: jthrowable) {
    jni_fn!(env, Throw)(env, throwable);
}

/// `d2c_throw_exception`
pub unsafe fn throw_exception(env: Env, class_name: &[u8], message: &[u8]) {
    let clz = with_cstr(class_name, |c| jni_fn!(env, FindClass)(env, c));
    if !clz.is_null() {
        with_cstr(message, |m| jni_fn!(env, ThrowNew)(env, clz, m));
    }
}

/// `d2c_is_instance_of(env, instance, clz)` — `null` is never an instance.
#[inline]
pub unsafe fn is_instance_of(env: Env, instance: jobject, clz: jclass) -> bool {
    if instance.is_null() {
        false
    } else {
        jni_fn!(env, IsInstanceOf)(env, instance, clz)
    }
}

/// `d2c_is_instance_of(env, instance, class_name)`
///
/// `name` is a plain byte literal or, under `--obfuscate`, a [`crate::Name`]
/// (see [`crate::NameArg`]). The exception types a handler matches on identify
/// the app's code as much as the classes a call resolves do, so they are
/// encrypted by the same pass. The decode happens after the cast/instance check
/// that can succeed without it, so the failure path is the only one that pays.
#[inline]
pub unsafe fn is_instance_of_name<N: crate::NameArg>(env: Env, instance: jobject, name: N) -> bool {
    let mut scratch = [0u8; crate::MAX_NAME];
    let Some(len) = name.decode(&mut scratch) else {
        return false;
    };
    let clz = with_cstr(&scratch[..len], |c| jni_fn!(env, FindClass)(env, c));
    if clz.is_null() {
        return false;
    }
    let result = is_instance_of(env, instance, clz);
    delete_local_ref(env, clz);
    result
}

/// `d2c_is_same_object`
#[inline]
pub unsafe fn is_same_object(env: Env, obj1: jobject, obj2: jobject) -> bool {
    if obj1 == obj2 {
        return true;
    }
    if !obj1.is_null() && !obj2.is_null() {
        jni_fn!(env, IsSameObject)(env, obj1, obj2)
    } else {
        false
    }
}

/// `d2c_check_cast` — true when a `ClassCastException` was thrown.
///
/// This calls JNI `IsInstanceOf` **directly**, exactly like the C++
/// `d2c_check_cast` did, and *not* `is_instance_of` below. The difference is
/// `null`: ART's `IsInstanceOf` answers true for a null object, because any
/// reference type may hold `null`, so `(Foo) null` is a legal cast. Routing this
/// through the `instance-of` helper (where null is not an instance of anything,
/// which is what that bytecode needs) made every cast of a null value throw a
/// `ClassCastException` naming the *target* class -- which is precisely the
/// `java.lang.ClassCastException: q5/a` a real device printed from inside
/// Compose's `CrashScreen$lambda$10`, where an `as` cast legitimately sees null
/// on the first composition.
#[inline]
pub unsafe fn check_cast<N: crate::NameArg>(
    env: Env,
    instance: jobject,
    clz: jclass,
    name: N,
) -> bool {
    if jni_fn!(env, IsInstanceOf)(env, instance, clz) {
        return false;
    }
    // The name is only needed for the exception message, so the decode is on the
    // failing path: a cast that succeeds -- every cast on a hot path -- does not
    // pay for obfuscation at all.
    let mut scratch = [0u8; crate::MAX_NAME];
    let class_name = match name.decode(&mut scratch) {
        Some(len) => &scratch[..len],
        None => &b"?"[..],
    };
    throw_exception(env, b"java/lang/ClassCastException", class_name);
    true
}

/// `D2C_NOT_NULL(obj)` — true when a `NullPointerException` was thrown.
#[inline]
pub unsafe fn not_null_or_throw(env: Env, obj: jobject) -> bool {
    if obj.is_null() {
        throw_exception(
            env,
            b"java/lang/NullPointerException",
            b"NullPointerException",
        );
        true
    } else {
        false
    }
}

/// `env->NewStringUTF(bytes)` for a plain (non-obfuscated) literal.
#[inline]
pub unsafe fn new_string_utf(env: Env, bytes: &[u8]) -> jstring {
    with_cstr(bytes, |c| jni_fn!(env, NewStringUTF)(env, c))
}

/// `env->AllocObject(clz)` — the `new-instance` helper.
#[inline]
pub unsafe fn alloc_object(env: Env, clz: jclass) -> jobject {
    jni_fn!(env, AllocObject)(env, clz)
}

/// `env->MonitorEnter(obj)`; the caller turns the status into an exception check.
#[inline]
pub unsafe fn monitor_enter(env: Env, obj: jobject) -> jint {
    jni_fn!(env, MonitorEnter)(env, obj)
}

/// `env->MonitorExit(obj)`
#[inline]
pub unsafe fn monitor_exit(env: Env, obj: jobject) -> jint {
    jni_fn!(env, MonitorExit)(env, obj)
}

/// Construct `jvalue` operands for `Call*MethodA`. Union field initialisation is
/// safe; only *reading* a union field needs `unsafe`.
///
/// The generated code keeps every int-like register in a single `jint` slot
/// (the same thing the C++ writer did, where the `jvalue` union truncated
/// implicitly), so the narrow constructors take `jint` and narrow here.
#[inline]
pub fn jv_boolean(v: jint) -> jvalue {
    jvalue { z: v != 0 }
}
#[inline]
pub fn jv_byte(v: jint) -> jvalue {
    jvalue { b: v as jbyte }
}
#[inline]
pub fn jv_char(v: jint) -> jvalue {
    jvalue { c: v as jchar }
}
#[inline]
pub fn jv_short(v: jint) -> jvalue {
    jvalue { s: v as jshort }
}
#[inline]
pub fn jv_int(v: jint) -> jvalue {
    jvalue { i: v }
}
#[inline]
pub fn jv_long(v: jlong) -> jvalue {
    jvalue { j: v }
}
#[inline]
pub fn jv_float(v: jfloat) -> jvalue {
    jvalue { f: v }
}
#[inline]
pub fn jv_double(v: jdouble) -> jvalue {
    jvalue { d: v }
}
#[inline]
pub fn jv_object(v: jobject) -> jvalue {
    jvalue { l: v }
}
