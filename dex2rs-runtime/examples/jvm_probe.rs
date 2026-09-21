//! Host-JVM probe for the JNI helpers of the runtime.
//!
//! The generated code's correctness is mostly a property of this crate, not of
//! the emitter, and the bugs that reach a device have been in exactly these
//! helpers (`check_cast` on a null value threw the `ClassCastException` that
//! killed `CrashActivityKt.CrashScreen$lambda$10`). A `cargo test` cannot reach
//! them, because JNI needs a real VM.
//!
//! So this example is built as a `cdylib` and loaded by `tools/jvm/Probe.java`
//! in a desktop JVM (`tools/host_jvm_check.sh` drives the two). It calls the
//! runtime functions with real JNI objects and reports what they did, which
//! makes the difference between "cast of null" (legal) and "cast of the wrong
//! type" (must throw) checkable in a second, without a phone or an emulator.
//!
//! The probe returns `JNI_TRUE`/`JNI_FALSE` as an int: `1` means the helper
//! threw, `0` means it did not. Any pending exception is cleared before
//! returning so the Java side can keep running and report it.

// Same allowance as the runtime crate itself: every JNI call is `unsafe`, so
// wrapping each one again would only add noise.
#![allow(unsafe_op_in_unsafe_fn)]

use std::ffi::CString;

use dex2rs_runtime as d2c;
use dex2rs_runtime::jni_fn;

/// `env->FindClass(name)`, with the name as a Rust string.
unsafe fn class_of(env: d2c::Env, name: &str) -> d2c::jni_sys::jclass {
    let name = CString::new(name).expect("class name has no NUL");
    jni_fn!(env, FindClass)(env, name.as_ptr())
}

/// Run one cast and report whether it threw, leaving no pending exception.
unsafe fn report(env: d2c::Env, threw: bool) -> d2c::jni_sys::jint {
    if threw {
        jni_fn!(env, ExceptionClear)(env);
        1
    } else {
        0
    }
}

/// `(String) null` — legal; a `ClassCastException` here is the bug that broke
/// the app's crash screen.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_checkCastNull(
    env: d2c::Env,
    _class: d2c::jni_sys::jclass,
) -> d2c::jni_sys::jint {
    let target = class_of(env, "java/lang/String");
    let threw = d2c::check_cast(env, core::ptr::null_mut(), target, b"java/lang/String");
    d2c::delete_local_ref(env, target);
    report(env, threw)
}

/// `(String) new Object()` — must throw.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_checkCastWrongType(
    env: d2c::Env,
    _class: d2c::jni_sys::jclass,
) -> d2c::jni_sys::jint {
    let object_class = class_of(env, "java/lang/Object");
    let target = class_of(env, "java/lang/String");
    let object = jni_fn!(env, AllocObject)(env, object_class);
    let threw = d2c::check_cast(env, object, target, b"java/lang/String");
    d2c::delete_local_ref(env, object);
    d2c::delete_local_ref(env, target);
    d2c::delete_local_ref(env, object_class);
    report(env, threw)
}

/// `(String) "x"` — must not throw.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_checkCastSameType(
    env: d2c::Env,
    _class: d2c::jni_sys::jclass,
) -> d2c::jni_sys::jint {
    let target = class_of(env, "java/lang/String");
    let text = CString::new("x").expect("literal has no NUL");
    let value = jni_fn!(env, NewStringUTF)(env, text.as_ptr());
    let threw = d2c::check_cast(env, value, target, b"java/lang/String");
    d2c::delete_local_ref(env, value);
    d2c::delete_local_ref(env, target);
    report(env, threw)
}

/// The landing pad's whole job: take the pending exception and ask whether it is
/// an instance of each `catch` type. Returns `(matched RuntimeException) << 1 |
/// (matched Error)`, so `2` is the correct answer.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_exceptionDispatch(
    env: d2c::Env,
    _class: d2c::jni_sys::jclass,
) -> d2c::jni_sys::jint {
    d2c::throw_exception(env, b"java/lang/ClassCastException", b"probe");
    let exception = d2c::get_pending_exception(env);
    // A `ClassCastException` is a `RuntimeException` but not an `Error`.
    let runtime = d2c::is_instance_of_name(env, exception, b"java/lang/RuntimeException");
    let error = d2c::is_instance_of_name(env, exception, b"java/lang/Error");
    d2c::delete_local_ref(env, exception);
    ((runtime as d2c::jni_sys::jint) << 1) | (error as d2c::jni_sys::jint)
}

/// Resolve `String.length()` the way generated code does and call it: the
/// resolve cache plus `Call<Type>MethodA` marshalling. Returns the length
/// (`5`), or `-1` when a resolution failed.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_callResolvedMethod(
    env: d2c::Env,
    _class: d2c::jni_sys::jclass,
) -> d2c::jni_sys::jint {
    let mut clz: d2c::jni_sys::jclass = core::ptr::null_mut();
    if d2c::resolve_class(env, &mut clz, b"java/lang/String") {
        return -1;
    }
    let mut mid: d2c::jni_sys::jmethodID = core::ptr::null_mut();
    if d2c::resolve_method(
        env,
        &mut clz,
        &mut mid,
        false,
        b"java/lang/String",
        b"length",
        b"()I",
    ) {
        return -1;
    }
    let text = CString::new("hello").expect("literal has no NUL");
    let value = jni_fn!(env, NewStringUTF)(env, text.as_ptr());
    let no_args: [d2c::jni_sys::jvalue; 0] = [];
    let length = d2c::call_int_method(env, d2c::CALL_VIRTUAL, value, clz, mid, no_args.as_ptr());
    d2c::delete_local_ref(env, value);
    length
}

/// The `--obfuscate` path, fixed-key form: the emitter writes a `const-string` as its bytes plus
/// a NUL, XORs the whole array with [`d2c::OBFUSCATE_KEY`] at compile time
/// (`d2c::obfuscate`, a `const fn`) and the runtime XORs it back before
/// `NewStringUTF` (`d2c::new_string_utf_obf`). Returning the string lets Java
/// compare it with the literal it was built from, so the round trip and the
/// escaping of non-ASCII, backslash and quote bytes are all checked at once.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_obfuscatedString(
    env: d2c::Env,
    _class: d2c::jni_sys::jclass,
) -> d2c::jni_sys::jstring {
    // The plaintext is `h`, U+00E9, `llo`, space, backslash, space, quote,
    // space, U+1F41B (an emoji) -- the awkward cases. The emoji is written as
    // the CESU-8 surrogate pair the emitter produces (`util::mutf8`), not as
    // four-byte UTF-8, and the trailing NUL is part of the array exactly like
    // the emitter's literal.
    let stored = d2c::obfuscate(
        b"h\xc3\xa9llo \\ \" \xed\xa0\xbd\xed\xb0\x9b\x00",
        d2c::OBFUSCATE_KEY,
    );
    d2c::new_string_utf_obf(env, &stored)
}

/// The `--obfuscate` form that actually ships: what `dex2rs-core` emits, byte for
/// byte -- ciphertext generated ahead of time, plus the seed it derived from the
/// plaintext.
///
/// `obfuscatedString` above hand-builds the ciphertext with the runtime's own
/// `obfuscate`, so it cannot notice the generator and the runtime disagreeing:
/// both sides of that comparison come from the runtime. Here the bytes come from
/// the generator, and the seed has to hash back to them the way the runtime
/// assumes. It is the same plaintext as `obfuscatedString` so the two probes
/// cover one string through both emission forms.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_obfuscatedStringSeeded(
    env: d2c::Env,
    _class: d2c::jni_sys::jclass,
) -> d2c::jni_sys::jstring {
    d2c::new_string_utf_obf_seeded(
        env,
        b"8eJ\xef(\x0a2N\x19\xab\xc3d\x9b\xc3\xb3P\x8c7",
        0xa8de7fc3,
    )
}

/// The `--dynamic-register` path end to end: build a `JNINativeMethod` with the
/// runtime's helper and hand it to `RegisterNatives`, then let Java call the
/// method. The generated `dynamic_register.rs` does exactly this for every
/// converted class, and `RegisterNatives` fails when the name or the signature
/// does not match a declared method -- so a bad table is caught here rather than
/// as a hard-to-read crash on a device.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_registerDynamicProbe(
    env: d2c::Env,
    _class: d2c::jni_sys::jclass,
) -> d2c::jboolean {
    let table = [d2c::native_method(
        c"dynamicProbe",
        c"(I)I",
        Java_Probe_dynamicProbe as *const (),
    )];
    if d2c::register_natives(env, b"Probe", &table) {
        1
    } else {
        0
    }
}

/// The method `Java_Probe_registerDynamicProbe` registers; `20` in, `41` out.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_dynamicProbe(
    _env: d2c::Env,
    _class: d2c::jni_sys::jclass,
    x: d2c::jni_sys::jint,
) -> d2c::jni_sys::jint {
    x * 2 + 1
}

/// `invoke-polymorphic` against a real `MethodHandle`: the runtime boxes both
/// `int` arguments, drives `MethodHandle.invokeWithArguments` and unboxes the
/// result. Returns the integer the handle produced, or `-1` on any failure.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_invokeMethodHandle(
    env: d2c::Env,
    _class: d2c::jni_sys::jclass,
    handle: d2c::jni_sys::jobject,
    a: d2c::jni_sys::jint,
    b: d2c::jni_sys::jint,
) -> d2c::jni_sys::jint {
    // Exactly the array generated code builds for `invokeExact(a, b)`, and the
    // prototype from the instruction's proto operand.
    let args = [d2c::jv_int(a), d2c::jv_int(b)];
    let result = d2c::invoke_polymorphic(
        env,
        handle,
        d2c::HANDLE_METHOD,
        b"invokeExact",
        b"(II)I",
        args.as_ptr(),
    );
    if result.is_null() {
        jni_fn!(env, ExceptionClear)(env);
        return -1;
    }
    let value = d2c::unbox_int(env, result);
    d2c::delete_local_ref(env, result);
    if d2c::exception_check(env) {
        jni_fn!(env, ExceptionClear)(env);
        return -1;
    }
    value
}

/// `invoke-polymorphic` against a real `VarHandle`: the access mode is named the
/// way the dex names it (`getAndAdd`), the first argument is an `int[]`, and the
/// handle must see the caller's array so the mutation is visible in Java.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_invokeVarHandle(
    env: d2c::Env,
    _class: d2c::jni_sys::jclass,
    handle: d2c::jni_sys::jobject,
    array: d2c::jni_sys::jobject,
    index: d2c::jni_sys::jint,
    delta: d2c::jni_sys::jint,
) -> d2c::jni_sys::jint {
    let args = [
        d2c::jv_object(array),
        d2c::jv_int(index),
        d2c::jv_int(delta),
    ];
    let result = d2c::invoke_polymorphic(
        env,
        handle,
        d2c::HANDLE_VAR,
        b"getAndAdd",
        b"([III)I",
        args.as_ptr(),
    );
    if result.is_null() {
        jni_fn!(env, ExceptionClear)(env);
        return -1;
    }
    let value = d2c::unbox_int(env, result);
    d2c::delete_local_ref(env, result);
    if d2c::exception_check(env) {
        jni_fn!(env, ExceptionClear)(env);
        return -1;
    }
    value
}

/// Can generated code build a `MethodHandle`/`MethodType` from JNI?
///
/// `const-method-handle` and `const-method-type` have no JNI equivalent, so the
/// runtime resolves them through `java.lang.invoke`. `MethodHandles.lookup()` is
/// caller-sensitive and a JNI frame is not a Java frame, so the three questions
/// are: which class the VM attributes the returned `Lookup` to, whether
/// `findStatic` can therefore reach the members the dex could reach, and whether
/// a `MethodType` can be built from a descriptor at all.
///
/// Returns a bit field, or a negative step number when a lookup *id* is missing
/// (calling a null `jmethodID` aborts the VM, so every one is checked and the
/// failure is reported instead):
///
/// | bit | meaning |
/// |---|---|
/// | 1 | `lookupClass()` is the class that declared the native method |
/// | 2 | `findStatic` found this class's public static method |
/// | 4 | `findStatic` also found its *private* static method |
/// | 8 | `MethodType.methodType` built a type from a descriptor |
/// | 16 | `const-method-handle` built an `instance-get` getter and read 7 |
/// | 32 | `const-method-handle` built an `invoke-static` handle that returned 3 |
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_lookupProbe(
    env: d2c::Env,
    _class: d2c::jni_sys::jclass,
) -> d2c::jni_sys::jint {
    let mut bits: d2c::jni_sys::jint = 0;

    // --- MethodHandles.lookup() -------------------------------------------------
    let Some((handles, lookup_id)) = d2c::resolve_static_method(
        env,
        b"java/lang/invoke/MethodHandles",
        b"lookup",
        b"()Ljava/lang/invoke/MethodHandles$Lookup;",
    ) else {
        return -1;
    };
    let lookup_obj =
        jni_fn!(env, CallStaticObjectMethodA)(env, handles, lookup_id, core::ptr::null());
    if lookup_obj.is_null() {
        jni_fn!(env, ExceptionClear)(env);
        return -2;
    }

    // --- which class is it attributed to? ---------------------------------------
    let Some(lookup_class) = d2c::resolve_instance_method(
        env,
        b"java/lang/invoke/MethodHandles$Lookup",
        b"lookupClass",
        b"()Ljava/lang/Class;",
    ) else {
        d2c::delete_local_ref(env, lookup_obj);
        return -3;
    };
    let Some(get_name) =
        d2c::resolve_instance_method(env, b"java/lang/Class", b"getName", b"()Ljava/lang/String;")
    else {
        d2c::delete_local_ref(env, lookup_obj);
        return -4;
    };
    let declaring =
        jni_fn!(env, CallObjectMethodA)(env, lookup_obj, lookup_class, core::ptr::null());
    if !declaring.is_null() {
        let name = jni_fn!(env, CallObjectMethodA)(env, declaring, get_name, core::ptr::null());
        if !name.is_null()
            && let Some(equals) = d2c::resolve_instance_method(
                env,
                b"java/lang/String",
                b"equals",
                b"(Ljava/lang/Object;)Z",
            )
        {
            let want = new_string(env, "Probe");
            let args = [d2c::jv_object(want)];
            // Content, not identity: two `String` objects are different oops.
            if jni_fn!(env, CallBooleanMethodA)(env, name, equals, args.as_ptr()) {
                bits |= 1;
            }
            d2c::delete_local_ref(env, want);
        }
        jni_fn!(env, ExceptionClear)(env);
        d2c::delete_local_ref(env, declaring);
    }

    // --- MethodType from a descriptor -------------------------------------------
    let method_type = d2c::method_type(env, b"(II)I");
    if method_type.is_null() {
        jni_fn!(env, ExceptionClear)(env);
    } else {
        bits |= 8;
    }

    // --- findStatic, public and private -----------------------------------------
    let find_static = d2c::resolve_instance_method(
        env,
        b"java/lang/invoke/MethodHandles$Lookup",
        b"findStatic",
        b"(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
    );
    let probe_class = class_of(env, "Probe");
    if let (Some(find_static), false) = (find_static, method_type.is_null()) {
        for (method, bit) in [("add", 2), ("secret", 4)] {
            let name = new_string(env, method);
            let args = [
                d2c::jv_object(probe_class),
                d2c::jv_object(name),
                d2c::jv_object(method_type),
            ];
            let handle =
                jni_fn!(env, CallObjectMethodA)(env, lookup_obj, find_static, args.as_ptr());
            if !handle.is_null() {
                bits |= bit;
                d2c::delete_local_ref(env, handle);
            } else {
                jni_fn!(env, ExceptionClear)(env);
            }
            d2c::delete_local_ref(env, name);
        }
    }

    // --- const-method-handle: a field setter/getter pair -------------------------
    // `AllocObject` does not run field initializers, so the setter writes the
    // value the getter then has to read back: two kinds, one check.
    let receiver = jni_fn!(env, AllocObject)(env, probe_class);
    let mut wrote = false;
    if let Some(put) = handle_of(
        env,
        d2c::HANDLE_KIND_INSTANCE_PUT,
        b"LProbe;",
        b"seven",
        b"I",
    ) {
        let args = [d2c::jv_object(receiver), d2c::jv_int(7)];
        let _ = d2c::invoke_polymorphic(
            env,
            put,
            d2c::HANDLE_METHOD,
            b"invokeExact",
            b"(LProbe;I)V",
            args.as_ptr(),
        );
        wrote = !jni_fn!(env, ExceptionCheck)(env);
        jni_fn!(env, ExceptionClear)(env);
        d2c::delete_local_ref(env, put);
    }
    if wrote
        && let Some(get) = handle_of(
            env,
            d2c::HANDLE_KIND_INSTANCE_GET,
            b"LProbe;",
            b"seven",
            b"I",
        )
    {
        let args = [d2c::jv_object(receiver)];
        let value = d2c::invoke_polymorphic(
            env,
            get,
            d2c::HANDLE_METHOD,
            b"invokeExact",
            b"(LProbe;)I",
            args.as_ptr(),
        );
        if !value.is_null() && d2c::unbox_int(env, value) == 7 {
            bits |= 16;
        }
        d2c::delete_local_ref(env, value);
        jni_fn!(env, ExceptionClear)(env);
        d2c::delete_local_ref(env, get);
    }
    d2c::delete_local_ref(env, receiver);

    // --- const-method-handle: a static method reference --------------------------
    if let Some(add) = handle_of(
        env,
        d2c::HANDLE_KIND_INVOKE_STATIC,
        b"LProbe;",
        b"add",
        b"(II)I",
    ) {
        let args = [d2c::jv_int(1), d2c::jv_int(2)];
        let sum = d2c::invoke_polymorphic(
            env,
            add,
            d2c::HANDLE_METHOD,
            b"invokeExact",
            b"(II)I",
            args.as_ptr(),
        );
        if !sum.is_null() && d2c::unbox_int(env, sum) == 3 {
            bits |= 32;
        }
        d2c::delete_local_ref(env, sum);
        jni_fn!(env, ExceptionClear)(env);
        d2c::delete_local_ref(env, add);
    }

    d2c::delete_local_ref(env, probe_class);
    d2c::delete_local_ref(env, method_type);
    d2c::delete_local_ref(env, lookup_obj);
    bits
}

/// One `const-method-handle` constant, or `None` with the exception cleared.
unsafe fn handle_of(
    env: d2c::Env,
    kind: u8,
    class: &[u8],
    name: &[u8],
    desc: &[u8],
) -> Option<d2c::jni_sys::jobject> {
    let handle = d2c::method_handle(env, kind, class, name, desc);
    if handle.is_null() {
        jni_fn!(env, ExceptionClear)(env);
        return None;
    }
    Some(handle)
}

/// `env->NewStringUTF` for a Rust literal.
unsafe fn new_string(env: d2c::Env, text: &str) -> d2c::jni_sys::jstring {
    let text = CString::new(text).expect("literal has no NUL");
    jni_fn!(env, NewStringUTF)(env, text.as_ptr())
}

/// `instance-of` is the opposite rule: `null` is not an instance of anything.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_Probe_instanceOfNull(
    env: d2c::Env,
    _class: d2c::jni_sys::jclass,
) -> d2c::jni_sys::jint {
    let target = class_of(env, "java/lang/String");
    let result = d2c::is_instance_of(env, core::ptr::null_mut(), target);
    d2c::delete_local_ref(env, target);
    if result { 1 } else { 0 }
}
