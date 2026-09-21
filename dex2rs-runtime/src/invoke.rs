//! `java.lang.invoke` dispatch — the runtime half of `invoke-polymorphic`.
//!
//! `invoke-polymorphic` is how a dex calls a *signature-polymorphic* method:
//! `MethodHandle.invoke`/`invokeExact` and every `VarHandle` access mode
//! (`get`, `set`, `getAndAdd`, `compareAndSet`, ...). D8 keeps that instruction
//! in ordinary release output — it is not something only `--no-desugaring` or a
//! hand-written dex produces — so any app touching `java.lang.invoke`, directly
//! or through a library that does, reaches this module.
//!
//! A JNI call cannot reproduce a polymorphic call site on its own: the JVM
//! dispatches it on the *caller's* descriptor, and `Call<Type>MethodA` can only
//! name the erased one (`([Ljava/lang/Object;)Ljava/lang/Object;`). Both sides of
//! the bridge used here are ordinary, public, **non**-polymorphic methods, so
//! they are exactly callable from JNI:
//!
//! * `MethodHandle.invoke`/`invokeExact` → `MethodHandle.invokeWithArguments`,
//!   which performs the `asType` adaptation the call site would have performed.
//! * each `VarHandle` access mode → [`VarHandle.toMethodHandle`] with
//!   `AccessMode.valueFromMethodName`, then the same `invokeWithArguments`.
//!   (`VarHandle` is API 33+, and `toMethodHandle` shipped with it.)
//!
//! Arguments are boxed *here*, not in the emitter, so generated code only has to
//! build the same `jvalue[]` it builds for an ordinary call: one call site,
//! `d2c::invoke_polymorphic(env, target, kind, name, proto, __args.as_ptr())`, and
//! one unboxing call per return descriptor.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use jni_sys::*;

use crate::Env;
use crate::jni::{delete_local_ref, new_local_ref, new_string_utf, with_cstr};
use crate::ops::{self, CALL_VIRTUAL};

/// `MethodHandle.invoke` / `invokeExact` / `invokeBasic`.
pub const HANDLE_METHOD: u8 = 0;
/// A `VarHandle` access mode, named by the dex method (`getAndAdd`, ...).
pub const HANDLE_VAR: u8 = 1;

// ------------------------------------------------------------------ caches ----

/// `(class, name, signature)` → `(global jclass, jmethodID)`, resolved once.
static METHODS: Mutex<BTreeMap<Vec<u8>, (usize, usize)>> = Mutex::new(BTreeMap::new());

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn key(class: &[u8], name: &[u8], signature: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(class.len() + name.len() + signature.len() + 2);
    out.extend_from_slice(class);
    out.push(b'-');
    out.extend_from_slice(name);
    out.push(b'(');
    out.extend_from_slice(signature);
    out
}

/// Look up a method once and keep a global class reference beside it.
///
/// `signature` doubles as the cache key and as the JNI descriptor, so the two
/// can never disagree. On failure the returned id is null with a pending
/// `NoSuchMethodError`/`NoClassDefFoundError`, which the generated code's
/// exception check turns into a jump to its handler.
unsafe fn method(
    env: Env,
    class: &[u8],
    name: &[u8],
    signature: &[u8],
    is_static: bool,
) -> (jclass, jmethodID) {
    let k = key(class, name, signature);
    if let Some(&(clz, mid)) = lock(&METHODS).get(&k) {
        return (clz as jclass, mid as jmethodID);
    }

    let local = with_cstr(class, |c| jni_fn!(env, FindClass)(env, c));
    if local.is_null() {
        return (core::ptr::null_mut(), core::ptr::null_mut());
    }
    let id = with_cstr(name, |n| {
        with_cstr(signature, |s| {
            if is_static {
                jni_fn!(env, GetStaticMethodID)(env, local, n, s)
            } else {
                jni_fn!(env, GetMethodID)(env, local, n, s)
            }
        })
    });
    if id.is_null() {
        delete_local_ref(env, local);
        return (core::ptr::null_mut(), core::ptr::null_mut());
    }
    let global = jni_fn!(env, NewGlobalRef)(env, local);
    delete_local_ref(env, local);
    if global.is_null() {
        return (core::ptr::null_mut(), core::ptr::null_mut());
    }
    lock(&METHODS).insert(k, (global as usize, id as usize));
    (global as jclass, id)
}

/// A static method (`Integer.valueOf`, `MethodHandles.lookup`, ...).
unsafe fn static_method(
    env: Env,
    class: &[u8],
    name: &[u8],
    signature: &[u8],
) -> (jclass, jmethodID) {
    unsafe { method(env, class, name, signature, true) }
}

/// An instance method (`MethodHandle.invokeWithArguments`, ...).
unsafe fn instance_method(env: Env, class: &[u8], name: &[u8], signature: &[u8]) -> jmethodID {
    unsafe { method(env, class, name, signature, false).1 }
}

/// `(class, jmethodID)` for a static method, or `None` with a pending exception.
///
/// The `jclass` is the cache's **global** reference and must not be released;
/// only pass it to JNI calls.
pub unsafe fn resolve_static_method(
    env: Env,
    class: &[u8],
    name: &[u8],
    signature: &[u8],
) -> Option<(jclass, jmethodID)> {
    let (clz, mid) = unsafe { static_method(env, class, name, signature) };
    if mid.is_null() {
        None
    } else {
        Some((clz, mid))
    }
}

/// `jmethodID` for an instance method, or `None` with a pending exception.
pub unsafe fn resolve_instance_method(
    env: Env,
    class: &[u8],
    name: &[u8],
    signature: &[u8],
) -> Option<jmethodID> {
    let (_, mid) = unsafe { method(env, class, name, signature, false) };
    if mid.is_null() { None } else { Some(mid) }
}

// ------------------------------------------------------------- descriptors ----

/// Split a call-site prototype (`(II)I`) into its parameter descriptors and the
/// return descriptor. Both `invoke-polymorphic` operands are prototypes, so this
/// is the only parsing this module needs.
pub fn split_proto(proto: &[u8]) -> (Vec<&[u8]>, &[u8]) {
    let Some(open) = proto.iter().position(|&c| c == b'(') else {
        return (Vec::new(), proto);
    };
    let Some(close) = proto[open..].iter().position(|&c| c == b')') else {
        return (Vec::new(), proto);
    };
    let close = open + close;
    let params = &proto[open + 1..close];
    let ret = &proto[close + 1..];

    let mut out = Vec::new();
    let mut i = 0;
    while i < params.len() {
        let start = i;
        while i < params.len() && params[i] == b'[' {
            i += 1;
        }
        if i >= params.len() {
            break;
        }
        if params[i] == b'L' {
            while i < params.len() && params[i] != b';' {
                i += 1;
            }
            if i < params.len() {
                i += 1; // include the ';'
            }
        } else {
            i += 1;
        }
        out.push(&params[start..i]);
    }
    (out, ret)
}

// ---------------------------------------------------------------- boxing ------

/// The `Class` object for a type descriptor, as a **local** reference (release
/// it with [`delete_local_ref`]).
///
/// Primitives come from the primitives cache (under the names `util::get_type`
/// produces, plus `Void`), and reference types — including array descriptors —
/// through `FindClass`, which accepts both.
unsafe fn class_for_descriptor(env: Env, desc: &[u8]) -> jclass {
    let primitive: &[u8] = match desc {
        [b'Z'] => b"Boolean",
        [b'B'] => b"Byte",
        [b'C'] => b"Char",
        [b'S'] => b"Short",
        [b'I'] => b"Int",
        [b'J'] => b"Long",
        [b'F'] => b"Float",
        [b'D'] => b"Double",
        [b'V'] => b"Void",
        _ => b"",
    };
    if !primitive.is_empty() {
        // `primitive_class_object` hands back a process-wide *global* reference,
        // while `FindClass` below produces a local one. Every caller in this
        // module releases what it gets, and `DeleteLocalRef` on a global
        // reference aborts the VM, so the global is copied into a local here and
        // the two cases are indistinguishable to the caller.
        let global = unsafe { crate::resolve::primitive_class_object(env, primitive) };
        return unsafe { new_local_ref(env, global) };
    }
    let name = match desc {
        [b'L', rest @ .., b';'] => rest,
        _ => desc,
    };
    with_cstr(name, |c| unsafe { jni_fn!(env, FindClass)(env, c) })
}

/// `MethodType.methodType(returnType, parameterTypes...)` for a descriptor.
///
/// Returns null with a pending exception when a type in the descriptor cannot be
/// resolved. The `(Class, Class[])` overload is the varargs one, so an empty
/// parameter array is also correct.
///
/// `proto` is a plain descriptor or, under `--obfuscate`, a [`crate::Name`] (see
/// [`crate::NameArg`]): a descriptor names the app's types as plainly as a class
/// name does, so it is encrypted by the same pass.
pub unsafe fn method_type<P: crate::NameArg>(env: Env, proto: P) -> jobject {
    let mut proto_scratch = [0u8; crate::MAX_NAME];
    let Some(proto_len) = proto.decode(&mut proto_scratch) else {
        return core::ptr::null_mut();
    };
    let proto = &proto_scratch[..proto_len];
    let (params, ret) = split_proto(proto);
    let (method_type_class, method_type_of) = static_method(
        env,
        b"java/lang/invoke/MethodType",
        b"methodType",
        b"(Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
    );
    if method_type_of.is_null() {
        return core::ptr::null_mut();
    }
    let ret_class = class_for_descriptor(env, ret);
    if ret_class.is_null() {
        return core::ptr::null_mut();
    }
    let class_class = with_cstr(b"java/lang/Class", |c| jni_fn!(env, FindClass)(env, c));
    if class_class.is_null() {
        delete_local_ref(env, ret_class);
        return core::ptr::null_mut();
    }
    let param_array = ops::new_object_array(env, params.len() as jsize, class_class);
    delete_local_ref(env, class_class);
    if param_array.is_null() {
        delete_local_ref(env, ret_class);
        return core::ptr::null_mut();
    }
    for (index, param) in params.iter().enumerate() {
        let clz = class_for_descriptor(env, param);
        if clz.is_null() {
            delete_local_ref(env, param_array);
            delete_local_ref(env, ret_class);
            return core::ptr::null_mut();
        }
        ops::set_object_array_elem(env, param_array, index as jsize, clz);
        delete_local_ref(env, clz);
    }
    let args = [jvalue { l: ret_class }, jvalue { l: param_array }];
    let result = jni_fn!(env, CallStaticObjectMethodA)(
        env,
        method_type_class,
        method_type_of,
        args.as_ptr(),
    );
    delete_local_ref(env, param_array);
    delete_local_ref(env, ret_class);
    result
}

/// The `MethodHandle` kinds a dex `const-method-handle` can name, as the DEX
/// format numbers them (`method_handle_item.handle_type`).
pub const HANDLE_KIND_STATIC_PUT: u8 = 0x00;
pub const HANDLE_KIND_STATIC_GET: u8 = 0x01;
pub const HANDLE_KIND_INSTANCE_PUT: u8 = 0x02;
pub const HANDLE_KIND_INSTANCE_GET: u8 = 0x03;
pub const HANDLE_KIND_INVOKE_STATIC: u8 = 0x04;
pub const HANDLE_KIND_INVOKE_INSTANCE: u8 = 0x05;
pub const HANDLE_KIND_INVOKE_DIRECT: u8 = 0x06;
pub const HANDLE_KIND_INVOKE_CONSTRUCTOR: u8 = 0x07;
pub const HANDLE_KIND_INVOKE_INTERFACE: u8 = 0x08;

/// `const-method-handle`: one `java.lang.invoke.MethodHandle` constant.
///
/// `desc` is a *field* descriptor for the four field kinds and a *method*
/// descriptor otherwise, which is what the dex's `method_handle_item` means by
/// its target. Everything else goes through `MethodHandles.Lookup`:
///
/// | kind | `Lookup` call |
/// |---|---|
/// | `static-get` / `static-put` | `findStaticGetter` / `findStaticSetter` |
/// | `instance-get` / `instance-put` | `findGetter` / `findSetter` |
/// | `invoke-static` | `findStatic` |
/// | `invoke-instance`, `invoke-interface` | `findVirtual` (which also takes an interface) |
/// | `invoke-direct` | `findSpecial` with the declaring class as the caller |
/// | `invoke-constructor` | `findConstructor` |
///
/// `MethodHandles.lookup()` is caller-sensitive, so the access the resulting
/// handle has is the access of the class the VM attributes this native frame to
/// — which is the converted class itself. A handle to a member that class could
/// not reach in the original dex throws the same `IllegalAccessException` it
/// would have there.
pub unsafe fn method_handle<C: crate::NameArg, Nm: crate::NameArg, D: crate::NameArg>(
    env: Env,
    kind: u8,
    class: C,
    name: Nm,
    desc: D,
) -> jobject {
    let mut class_scratch = [0u8; crate::MAX_NAME];
    let mut name_scratch = [0u8; crate::MAX_NAME];
    let mut desc_scratch = [0u8; crate::MAX_NAME];
    let (Some(class_len), Some(name_len), Some(desc_len)) = (
        class.decode(&mut class_scratch),
        name.decode(&mut name_scratch),
        desc.decode(&mut desc_scratch),
    ) else {
        return core::ptr::null_mut();
    };
    let class = &class_scratch[..class_len];
    let name = &name_scratch[..name_len];
    let desc = &desc_scratch[..desc_len];
    let (handles, lookup) = static_method(
        env,
        b"java/lang/invoke/MethodHandles",
        b"lookup",
        b"()Ljava/lang/invoke/MethodHandles$Lookup;",
    );
    if lookup.is_null() {
        return core::ptr::null_mut();
    }
    let lookup_obj = jni_fn!(env, CallStaticObjectMethodA)(env, handles, lookup, core::ptr::null());
    if lookup_obj.is_null() {
        return core::ptr::null_mut();
    }
    let target_class = class_for_descriptor(env, class);
    if target_class.is_null() {
        delete_local_ref(env, lookup_obj);
        return core::ptr::null_mut();
    }

    // The last parameter of the find* call: a field's type, a method type, or a
    // method type plus the special caller.
    let (find_name, signature): (&[u8], &[u8]) = match kind {
        HANDLE_KIND_STATIC_PUT => (b"findStaticSetter", b"(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
        HANDLE_KIND_STATIC_GET => (b"findStaticGetter", b"(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
        HANDLE_KIND_INSTANCE_PUT => (b"findSetter", b"(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
        HANDLE_KIND_INSTANCE_GET => (b"findGetter", b"(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
        HANDLE_KIND_INVOKE_STATIC => (b"findStatic", b"(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
        HANDLE_KIND_INVOKE_CONSTRUCTOR => (b"findConstructor", b"(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
        HANDLE_KIND_INVOKE_DIRECT => (b"findSpecial", b"(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"),
        _ => (b"findVirtual", b"(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;"),
    };
    let find = instance_method(
        env,
        b"java/lang/invoke/MethodHandles$Lookup",
        find_name,
        signature,
    );
    if find.is_null() {
        delete_local_ref(env, target_class);
        delete_local_ref(env, lookup_obj);
        return core::ptr::null_mut();
    }

    let is_field = kind <= HANDLE_KIND_INSTANCE_GET;
    let third = if is_field {
        class_for_descriptor(env, desc)
    } else {
        method_type(env, desc)
    };
    if third.is_null() {
        delete_local_ref(env, target_class);
        delete_local_ref(env, lookup_obj);
        return core::ptr::null_mut();
    }
    let name_string = new_string_utf(env, name);
    if name_string.is_null() {
        delete_local_ref(env, third);
        delete_local_ref(env, target_class);
        delete_local_ref(env, lookup_obj);
        return core::ptr::null_mut();
    }

    let result = if kind == HANDLE_KIND_INVOKE_CONSTRUCTOR {
        let args = [jvalue { l: target_class }, jvalue { l: third }];
        jni_fn!(env, CallObjectMethodA)(env, lookup_obj, find, args.as_ptr())
    } else if kind == HANDLE_KIND_INVOKE_DIRECT {
        // A private method (or a constructor, which does not reach here): the
        // special caller is the class that declares it.
        let args = [
            jvalue { l: target_class },
            jvalue {
                l: name_string as jobject,
            },
            jvalue { l: third },
            jvalue { l: target_class },
        ];
        jni_fn!(env, CallObjectMethodA)(env, lookup_obj, find, args.as_ptr())
    } else {
        let args = [
            jvalue { l: target_class },
            jvalue {
                l: name_string as jobject,
            },
            jvalue { l: third },
        ];
        jni_fn!(env, CallObjectMethodA)(env, lookup_obj, find, args.as_ptr())
    };

    delete_local_ref(env, name_string as jobject);
    delete_local_ref(env, third);
    delete_local_ref(env, target_class);
    delete_local_ref(env, lookup_obj);
    result
}

/// Box one call-site argument. `desc` is the first byte of its descriptor
/// (`I`, `J`, `L`, `[`, ...); a reference passes through untouched.
unsafe fn box_value(env: Env, desc: u8, value: jvalue) -> jobject {
    let (wrapper, signature): (&[u8], &[u8]) = match desc {
        b'Z' => (b"java/lang/Boolean", b"(Z)Ljava/lang/Boolean;"),
        b'B' => (b"java/lang/Byte", b"(B)Ljava/lang/Byte;"),
        b'C' => (b"java/lang/Character", b"(C)Ljava/lang/Character;"),
        b'S' => (b"java/lang/Short", b"(S)Ljava/lang/Short;"),
        b'I' => (b"java/lang/Integer", b"(I)Ljava/lang/Integer;"),
        b'J' => (b"java/lang/Long", b"(J)Ljava/lang/Long;"),
        b'F' => (b"java/lang/Float", b"(F)Ljava/lang/Float;"),
        b'D' => (b"java/lang/Double", b"(D)Ljava/lang/Double;"),
        // `L`/`[`: already an object.
        _ => return new_local_ref(env, value.l),
    };
    let (clz, mid) = static_method(env, wrapper, b"valueOf", signature);
    if mid.is_null() {
        return core::ptr::null_mut();
    }
    // `valueOf` takes exactly the primitive the union already holds, so the same
    // `jvalue` can be handed straight to the call.
    let args = [value];
    jni_fn!(env, CallStaticObjectMethodA)(env, clz, mid, args.as_ptr())
}

/// Unbox a returned object into the register type the emitter expects for that
/// descriptor. Each accessor mirrors the `call_*_method` helper the same
/// descriptor would have used, so the emitter's existing widening applies.
macro_rules! unboxers {
    ($( $name:ident, $wrapper:literal, $method:literal, $signature:literal, $ret:ty, $call:ident ),* $(,)?) => {
        $(
            /// Unbox a value returned by the JNI call above.
            #[inline]
            pub unsafe fn $name(env: Env, obj: jobject) -> $ret {
                if obj.is_null() {
                    return Default::default();
                }
                let mid = instance_method(env, $wrapper, $method, $signature);
                if mid.is_null() {
                    return Default::default();
                }
                ops::$call(
                    env,
                    CALL_VIRTUAL,
                    obj,
                    core::ptr::null_mut(),
                    mid,
                    core::ptr::null(),
                )
            }
        )*
    };
}

unboxers! {
    unbox_boolean, b"java/lang/Boolean", b"booleanValue", b"()Z", jboolean, call_boolean_method,
    unbox_byte, b"java/lang/Byte", b"byteValue", b"()B", jbyte, call_byte_method,
    unbox_char, b"java/lang/Character", b"charValue", b"()C", jchar, call_char_method,
    unbox_short, b"java/lang/Short", b"shortValue", b"()S", jshort, call_short_method,
    unbox_int, b"java/lang/Integer", b"intValue", b"()I", jint, call_int_method,
    unbox_long, b"java/lang/Long", b"longValue", b"()J", jlong, call_long_method,
    unbox_float, b"java/lang/Float", b"floatValue", b"()F", jfloat, call_float_method,
    unbox_double, b"java/lang/Double", b"doubleValue", b"()D", jdouble, call_double_method,
}

// --------------------------------------------------------------- dispatch -----

/// `VarHandle` access mode → a `MethodHandle` for that mode.
///
/// `AccessMode.valueFromMethodName` accepts the dex spelling verbatim
/// (`getAndAdd` → `GET_AND_ADD`), so no name table is needed here.
unsafe fn access_mode_handle(env: Env, vh: jobject, mode_name: &[u8]) -> jobject {
    let (mode_clz, value_from) = static_method(
        env,
        b"java/lang/invoke/VarHandle$AccessMode",
        b"valueFromMethodName",
        b"(Ljava/lang/String;)Ljava/lang/invoke/VarHandle$AccessMode;",
    );
    if value_from.is_null() {
        return core::ptr::null_mut();
    }
    let name = new_string_utf(env, mode_name);
    if name.is_null() {
        return core::ptr::null_mut();
    }
    let args = [jvalue { l: name as jobject }];
    let mode = jni_fn!(env, CallStaticObjectMethodA)(env, mode_clz, value_from, args.as_ptr());
    delete_local_ref(env, name as jobject);
    if mode.is_null() {
        return core::ptr::null_mut();
    }

    let to_handle = instance_method(
        env,
        b"java/lang/invoke/VarHandle",
        b"toMethodHandle",
        b"(Ljava/lang/invoke/VarHandle$AccessMode;)Ljava/lang/invoke/MethodHandle;",
    );
    if to_handle.is_null() {
        delete_local_ref(env, mode);
        return core::ptr::null_mut();
    }
    let args = [jvalue { l: mode }];
    let handle = jni_fn!(env, CallObjectMethodA)(env, vh, to_handle, args.as_ptr());
    delete_local_ref(env, mode);
    handle
}

/// One `invoke-polymorphic` instruction. `kind` is [`HANDLE_METHOD`] or
/// [`HANDLE_VAR`]; `mode_name` names the method as the dex does; `proto` is the
/// call-site prototype *without* the receiver (`(II)I`); `args` is the
/// `jvalue[]` of the call-site arguments.
///
/// Returns the boxed result (`null` for a void call site, or with an exception
/// pending on failure) — the caller unboxes it per the prototype's return type.
pub unsafe fn invoke_polymorphic<M: crate::NameArg, P: crate::NameArg>(
    env: Env,
    target: jobject,
    kind: u8,
    mode_name: M,
    proto: P,
    args: *const jvalue,
) -> jobject {
    let mut mode_scratch = [0u8; crate::MAX_NAME];
    let mut proto_scratch = [0u8; crate::MAX_NAME];
    let (Some(mode_len), Some(proto_len)) = (
        mode_name.decode(&mut mode_scratch),
        proto.decode(&mut proto_scratch),
    ) else {
        return core::ptr::null_mut();
    };
    let mode_name = &mode_scratch[..mode_len];
    let proto = &proto_scratch[..proto_len];
    let (params, _ret) = split_proto(proto);

    // The call-site argument array, boxed per parameter descriptor.
    let object_class = with_cstr(b"java/lang/Object", |c| jni_fn!(env, FindClass)(env, c));
    if object_class.is_null() {
        return core::ptr::null_mut();
    }
    let boxed = ops::new_object_array(env, params.len() as jsize, object_class);
    delete_local_ref(env, object_class);
    if boxed.is_null() {
        return core::ptr::null_mut();
    }
    for (index, param) in params.iter().enumerate() {
        let value = *args.add(index);
        let element = box_value(env, param[0], value);
        if element.is_null() {
            delete_local_ref(env, boxed);
            return core::ptr::null_mut();
        }
        ops::set_object_array_elem(env, boxed, index as jsize, element);
        delete_local_ref(env, element);
    }

    let handle = match kind {
        HANDLE_VAR => access_mode_handle(env, target, mode_name),
        _ => new_local_ref(env, target),
    };
    if handle.is_null() {
        delete_local_ref(env, boxed);
        return core::ptr::null_mut();
    }

    let invoke = instance_method(
        env,
        b"java/lang/invoke/MethodHandle",
        b"invokeWithArguments",
        b"([Ljava/lang/Object;)Ljava/lang/Object;",
    );
    if invoke.is_null() {
        delete_local_ref(env, handle);
        delete_local_ref(env, boxed);
        return core::ptr::null_mut();
    }
    let call_args = [jvalue { l: boxed }];
    let result = jni_fn!(env, CallObjectMethodA)(env, handle, invoke, call_args.as_ptr());
    delete_local_ref(env, handle);
    delete_local_ref(env, boxed);
    result
}

#[cfg(test)]
mod tests {
    use super::split_proto;

    fn params(proto: &str) -> Vec<String> {
        let (params, ret) = split_proto(proto.as_bytes());
        let mut out: Vec<String> = params
            .iter()
            .map(|p| String::from_utf8(p.to_vec()).unwrap())
            .collect();
        out.push(format!("->{}", String::from_utf8(ret.to_vec()).unwrap()));
        out
    }

    /// The two shapes a real `invoke-polymorphic` carries: `MethodHandle`,
    /// where the receiver is absent from the prototype, and a `VarHandle`
    /// access mode whose first *parameter* is an array.
    #[test]
    fn splits_call_site_prototypes() {
        assert_eq!(params("(II)I"), ["I", "I", "->I"]);
        assert_eq!(params("([III)I"), ["[I", "I", "I", "->I"]);
        assert_eq!(params("()V"), ["->V"]);
        assert_eq!(
            params("(Ljava/lang/String;I)Ljava/lang/String;"),
            ["Ljava/lang/String;", "I", "->Ljava/lang/String;"]
        );
        assert_eq!(
            params("([[J[Ljava/lang/Object;)V"),
            ["[[J", "[Ljava/lang/Object;", "->V"]
        );
    }

    /// A malformed prototype must not panic or loop: it yields no parameters.
    #[test]
    fn malformed_prototypes_are_empty() {
        assert!(split_proto(b"").0.is_empty());
        assert!(split_proto(b"I").0.is_empty());
        assert!(split_proto(b"(II").0.is_empty());
    }
}
