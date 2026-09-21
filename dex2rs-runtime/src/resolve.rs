//! Class / method / field resolution with process-wide caches — port of
//! `Dex2C.cpp`'s global maps plus `well_known_classes.cpp`'s primitive classes.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use jni_sys::*;

use crate::Env;
use crate::jni::{delete_local_ref, with_cstr};

/// `max_global_reference`: above this many global class references, resolution
/// falls back to per-call local references.
const MAX_GLOBAL_REFERENCE: usize = 1500;

static CLASS_CACHE: Mutex<BTreeMap<Vec<u8>, usize>> = Mutex::new(BTreeMap::new());
static METHOD_CACHE: Mutex<BTreeMap<(Vec<u8>, Vec<u8>, Vec<u8>), usize>> =
    Mutex::new(BTreeMap::new());
static FIELD_CACHE: Mutex<BTreeMap<(Vec<u8>, Vec<u8>, Vec<u8>), usize>> =
    Mutex::new(BTreeMap::new());

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The most bytes one class/field/method name may occupy once decoded.
///
/// `--obfuscate` keeps these names out of `libstub.so`, so the runtime has to
/// decode a name before it can ask for it, and a decode needs a bound. The
/// generator refuses to emit a name longer than this (`codegen::MAX_OBF_NAME`,
/// which `dex2rs-core/tests/compile.rs` asserts equals this constant), so the
/// bound is a guarantee rather than a guess: on a real app the longest name is
/// 468 bytes. See [`name_too_long`] for what happens if the two ever disagree.
pub const MAX_NAME: usize = 1024;

/// A class, field or method name, in either of the two forms generated code
/// passes.
///
/// Plain builds hand over the byte literal they always have; an `--obfuscate`
/// build hands over a [`Name`]. Both decode to the same bytes, so `resolve_*`
/// has one body for the two and the process-wide caches are keyed on the
/// plaintext either way -- an obfuscated build and a plain one resolve the same
/// classes, in the same order, from the same keys.
pub trait NameArg {
    /// Write the plaintext name into `scratch` and return its length.
    ///
    /// `None` means the name did not fit [`MAX_NAME`] and the caller has to
    /// report a failure rather than use half a name. Returning a length rather
    /// than a slice keeps the two forms in one signature without a lifetime tie
    /// to `scratch`, which a plain literal does not need and cannot express.
    fn decode(self, scratch: &mut [u8; MAX_NAME]) -> Option<usize>;
}

impl<const N: usize> NameArg for &[u8; N] {
    /// A literal: copy it in. The copy is what lets one signature take both
    /// forms; it is `N` bytes, on the first resolution of each call site.
    #[inline]
    fn decode(self, scratch: &mut [u8; MAX_NAME]) -> Option<usize> {
        if N > MAX_NAME {
            return None;
        }
        scratch[..N].copy_from_slice(self);
        Some(N)
    }
}

/// The other shape generated code and the runtime's own helpers pass: a plain
/// slice that is already resolved, rather than a literal this has to know the
/// length of at compile time.
impl NameArg for &[u8] {
    #[inline]
    fn decode(self, scratch: &mut [u8; MAX_NAME]) -> Option<usize> {
        if self.len() > MAX_NAME {
            return None;
        }
        scratch[..self.len()].copy_from_slice(self);
        Some(self.len())
    }
}

/// One name the generator encrypted: ciphertext the same length as the name,
/// plus the seed the runtime regenerates its keystream from.
///
/// What this defends against is `strings` on the `.so`, which is how the
/// remaining plaintext is found in practice: the class, field and method names
/// the reflection path passes to `FindClass`/`GetMethodID` are the ones that
/// identify the converted code, and on a real app there are 40 530 call sites
/// carrying them. Only the name's *bytes* are encrypted -- the ciphertext is
/// generated ahead of time, so the plaintext never exists in the generated crate
/// (see `codegen::name_arg`), and it is XORed back off here.
#[derive(Clone, Copy)]
pub struct Name {
    cipher: &'static [u8],
    seed: u32,
}

impl Name {
    /// The `--obfuscate` form of one name, as the generator emits it.
    pub const fn obf(cipher: &'static [u8], seed: u32) -> Self {
        Self { cipher, seed }
    }
}

impl NameArg for Name {
    #[inline]
    fn decode(self, scratch: &mut [u8; MAX_NAME]) -> Option<usize> {
        let len = self.cipher.len();
        if len >= MAX_NAME {
            return None;
        }
        // `keystream` is the same generator `codegen::keystream` mirrors; the
        // seed is derived from the name itself, so one known key cannot decode
        // the library in one sweep, and regenerating an unchanged app produces
        // byte-identical output.
        let keys = crate::keystream::<MAX_NAME>(self.seed);
        for (i, byte) in self.cipher.iter().enumerate() {
            scratch[i] = byte ^ keys[i];
        }
        Some(len)
    }
}

/// A name longer than [`MAX_NAME`].
///
/// Unreachable from generated code: the generator refuses to emit one, and
/// `tests/compile.rs` pins the two limits together. It is a hard failure rather
/// than a truncated name, because resolving half a class name would fail in a
/// way that looks like a missing class.
#[cold]
fn name_too_long() -> bool {
    log::error!(
        "dex2rs: a class/field/method name is longer than the runtime's MAX_NAME \
         ({MAX_NAME} bytes); the generated crate and this runtime disagree"
    );
    true
}

/// The app's class loader, remembered the first time a class resolves with a
/// non-null one.
///
/// `FindClass` resolves against the class loader of the *calling* frame, and a
/// thread attached from native code has no frame to take one from: it falls back
/// to the system loader, which cannot see the app's own classes. That is not an
/// edge case here -- it is precisely the situation a converted method is in when
/// the app's own native library calls back into it, and the failure is silent
/// apart from a pending `ClassNotFoundException` that makes the converted method
/// return early on whatever thread happened to get there first. Caching any app
/// class's loader (one app loader resolves every class in the app's dex files)
/// and retrying through it removes that dependence on thread identity.
static CLASS_LOADER: Mutex<Option<usize>> = Mutex::new(None);

fn find_class(env: Env, name: &[u8]) -> jclass {
    let clz = with_cstr(name, |c| unsafe { jni_fn!(env, FindClass)(env, c) });
    if !clz.is_null() {
        unsafe { remember_class_loader(env, clz) };
        return clz;
    }
    let found = unsafe {
        // Clear the `ClassNotFoundException` `FindClass` left pending before
        // trying anything else, or a successful retry would still hand the
        // caller a pending exception.
        jni_fn!(env, ExceptionClear)(env);
        load_via_cached_loader(env, name)
    };
    if !found.is_null() {
        // The smoking gun for the failure mode this fallback exists for: the
        // class is in the app and `FindClass` could not see it. Logged, not
        // silent, because the *absence* of these lines in a capture is what
        // rules this path out rather than leaving it assumed.
        log::warn!(
            "dex2rs: {} resolved only through the app class loader -- this thread's FindClass \
             could not see it (there is no Java frame on its stack), so without the fallback \
             the converted method would have returned early with a ClassNotFoundException \
             pending",
            String::from_utf8_lossy(name)
        );
    }
    found
}

/// Remember the class loader of `clz`, if we do not have one yet.
///
/// Called only after a *successful* `FindClass`, which is the only time a real
/// app loader is in hand. A `null` result means the boot loader
/// (`java.lang.Integer` and friends), which is useless for app classes, so it is
/// deliberately not cached -- the next resolution that goes through an app class
/// gets another chance to record a loader that can see them.
unsafe fn remember_class_loader(env: Env, clz: jclass) {
    if lock(&CLASS_LOADER).is_some() {
        return;
    }
    let class_class = with_cstr(b"java/lang/Class", |c| jni_fn!(env, FindClass)(env, c));
    if class_class.is_null() {
        jni_fn!(env, ExceptionClear)(env);
        return;
    }
    let get_loader = jni_fn!(env, GetMethodID)(
        env,
        class_class,
        c"getClassLoader".as_ptr(),
        c"()Ljava/lang/ClassLoader;".as_ptr(),
    );
    if get_loader.is_null() {
        jni_fn!(env, ExceptionClear)(env);
        delete_local_ref(env, class_class);
        return;
    }
    let loader =
        jni_fn!(env, CallObjectMethodA)(env, clz as jobject, get_loader, core::ptr::null());
    delete_local_ref(env, class_class);
    if loader.is_null() {
        // Boot class loader: nothing to remember.
        jni_fn!(env, ExceptionClear)(env);
        return;
    }
    let global = jni_fn!(env, NewGlobalRef)(env, loader);
    delete_local_ref(env, loader);
    if global.is_null() {
        return;
    }
    lock(&CLASS_LOADER).replace(global as usize);
}

/// `ClassLoader.loadClass(name)` through the remembered loader.
///
/// Returns `null` with the loader's own `ClassNotFoundException` pending when the
/// class really is not there, which is what the caller reports upwards.
unsafe fn load_via_cached_loader(env: Env, name: &[u8]) -> jclass {
    let Some(loader) = lock(&CLASS_LOADER).as_ref().copied() else {
        return core::ptr::null_mut();
    };

    // `loadClass` takes the binary name (`a.b.C`), not the JNI name (`a/b/C`).
    let mut binary = name.to_vec();
    for byte in binary.iter_mut() {
        if *byte == b'/' {
            *byte = b'.';
        }
    }

    let loader_class = with_cstr(b"java/lang/ClassLoader", |c| {
        jni_fn!(env, FindClass)(env, c)
    });
    if loader_class.is_null() {
        jni_fn!(env, ExceptionClear)(env);
        return core::ptr::null_mut();
    }
    let load = jni_fn!(env, GetMethodID)(
        env,
        loader_class,
        c"loadClass".as_ptr(),
        c"(Ljava/lang/String;)Ljava/lang/Class;".as_ptr(),
    );
    if load.is_null() {
        jni_fn!(env, ExceptionClear)(env);
        delete_local_ref(env, loader_class);
        return core::ptr::null_mut();
    }

    let clz = with_cstr(&binary, |n| {
        let jname = jni_fn!(env, NewStringUTF)(env, n);
        if jname.is_null() {
            return core::ptr::null_mut();
        }
        let args: [jvalue; 1] = [jvalue { l: jname }];
        let found = jni_fn!(env, CallObjectMethodA)(env, loader as jobject, load, args.as_ptr());
        delete_local_ref(env, jname);
        found
    });
    delete_local_ref(env, loader_class);
    clz
}

/// `CachePrimitiveClass`: `<wrapper>.TYPE` as a global reference.
fn primitive_class(env: Env, wrapper: jclass) -> jobject {
    unsafe {
        let fid = jni_fn!(env, GetStaticFieldID)(
            env,
            wrapper,
            c"TYPE".as_ptr(),
            c"Ljava/lang/Class;".as_ptr(),
        );
        if fid.is_null() {
            return core::ptr::null_mut();
        }
        let value = jni_fn!(env, GetStaticObjectField)(env, wrapper, fid);
        if value.is_null() {
            return core::ptr::null_mut();
        }
        let global = jni_fn!(env, NewGlobalRef)(env, value);
        delete_local_ref(env, value);
        global
    }
}

/// `WellKnownClasses::Init` + `cache_well_known_classes`: the eight primitive
/// `Class` objects, registered under the names `util.get_type()` produces for the
/// primitive descriptors (`I` -> `Int`, ...), which is what `const-class` of a
/// primitive compiles to.
pub unsafe fn init(env: Env) -> bool {
    const PRIMITIVES: [(&[u8], &[u8]); 9] = [
        // `Void` is here for `const-method-type`/`const-method-handle`, whose
        // descriptors may name `void` as a return or field type; `const-class`
        // of a primitive never produces it, but `MethodType.methodType` needs
        // the `Class` object all the same.
        (b"java/lang/Void", b"Void"),
        (b"java/lang/Boolean", b"Boolean"),
        (b"java/lang/Byte", b"Byte"),
        (b"java/lang/Short", b"Short"),
        (b"java/lang/Character", b"Char"),
        (b"java/lang/Integer", b"Int"),
        (b"java/lang/Long", b"Long"),
        (b"java/lang/Float", b"Float"),
        (b"java/lang/Double", b"Double"),
    ];

    for (wrapper, primitive_name) in PRIMITIVES {
        let clz = find_class(env, wrapper);
        if clz.is_null() {
            return false;
        }
        let global = primitive_class(env, clz);
        delete_local_ref(env, clz);
        if global.is_null() {
            return false;
        }
        lock(&CLASS_CACHE).insert(primitive_name.to_vec(), global as usize);
    }
    true
}

/// The `Class` object of a primitive, by the name the primitives cache uses
/// (`Int`, `Long`, `Void`, ...).
///
/// `init` fills that cache at load time, but a caller can reach here first —
/// `MethodType.methodType` needs a primitive `Class` before any generated method
/// has run, and the host-JVM probe library has no `JNI_OnLoad` at all. So this
/// falls back to resolving `<wrapper>.TYPE` on the spot rather than failing.
pub unsafe fn primitive_class_object(env: Env, primitive_name: &[u8]) -> jclass {
    if let Some(found) = lock(&CLASS_CACHE).get(primitive_name).copied() {
        return found as jclass;
    }
    let wrapper: &[u8] = match primitive_name {
        b"Void" => b"java/lang/Void",
        b"Boolean" => b"java/lang/Boolean",
        b"Byte" => b"java/lang/Byte",
        b"Char" => b"java/lang/Character",
        b"Short" => b"java/lang/Short",
        b"Int" => b"java/lang/Integer",
        b"Long" => b"java/lang/Long",
        b"Float" => b"java/lang/Float",
        b"Double" => b"java/lang/Double",
        _ => return core::ptr::null_mut(),
    };
    let clz = find_class(env, wrapper);
    if clz.is_null() {
        return core::ptr::null_mut();
    }
    let global = primitive_class(env, clz);
    delete_local_ref(env, clz);
    if global.is_null() {
        return core::ptr::null_mut();
    }
    lock(&CLASS_CACHE).insert(primitive_name.to_vec(), global as usize);
    global as jclass
}

/// The body of [`resolve_class`], for a plaintext name.
///
/// Self-contained: it re-checks `cached` because `resolve_method`/`resolve_field`
/// reach the class through here, and the per-call-site slot is what makes a warm
/// call free.
unsafe fn resolve_class_plain(env: Env, cached: *mut jclass, class_name: &[u8]) -> bool {
    if !(*cached).is_null() {
        return false;
    }
    if let Some(found) = lock(&CLASS_CACHE).get(class_name).copied() {
        *cached = found as jclass;
        return false;
    }

    let clz = find_class(env, class_name);
    if clz.is_null() {
        return true;
    }

    if lock(&CLASS_CACHE).len() < MAX_GLOBAL_REFERENCE {
        let global = jni_fn!(env, NewGlobalRef)(env, clz);
        if !global.is_null() {
            lock(&CLASS_CACHE).insert(class_name.to_vec(), global as usize);
            delete_local_ref(env, clz);
            *cached = global as jclass;
            return false;
        }
    }
    *cached = clz;
    false
}

/// `d2c_resolve_class` — returns true when an exception is pending.
///
/// `name` is either the byte literal a plain build passes or, under
/// `--obfuscate`, a [`Name`] that is decoded first. The early-out on `cached`
/// runs before the decode, so a slot that has already resolved costs nothing
/// extra however the name was encoded.
pub unsafe fn resolve_class<N: NameArg>(env: Env, cached: *mut jclass, name: N) -> bool {
    if !(*cached).is_null() {
        return false;
    }
    let mut scratch = [0u8; MAX_NAME];
    match name.decode(&mut scratch) {
        Some(len) => resolve_class_plain(env, cached, &scratch[..len]),
        None => name_too_long(),
    }
}

/// The body of [`resolve_method`], for plaintext names.
#[allow(clippy::too_many_arguments)]
unsafe fn resolve_method_plain(
    env: Env,
    cached_class: *mut jclass,
    cached_method: *mut jmethodID,
    is_static: bool,
    class_name: &[u8],
    method_name: &[u8],
    signature: &[u8],
) -> bool {
    if resolve_class_plain(env, cached_class, class_name) {
        return true;
    }

    let key = (
        class_name.to_vec(),
        method_name.to_vec(),
        signature.to_vec(),
    );
    if let Some(found) = lock(&METHOD_CACHE).get(&key).copied() {
        *cached_method = found as jmethodID;
        return false;
    }

    let method = with_cstr(method_name, |n| {
        with_cstr(signature, |s| {
            if is_static {
                jni_fn!(env, GetStaticMethodID)(env, *cached_class, n, s)
            } else {
                jni_fn!(env, GetMethodID)(env, *cached_class, n, s)
            }
        })
    });

    if method.is_null() {
        return true;
    }
    lock(&METHOD_CACHE).insert(key, method as usize);
    *cached_method = method;
    false
}

/// `d2c_resolve_method` — returns true when an exception is pending. See
/// [`resolve_class`] for what `class_name`/`method_name`/`signature` may be.
#[allow(clippy::too_many_arguments)]
pub unsafe fn resolve_method<C: NameArg, M: NameArg, S: NameArg>(
    env: Env,
    cached_class: *mut jclass,
    cached_method: *mut jmethodID,
    is_static: bool,
    class_name: C,
    method_name: M,
    signature: S,
) -> bool {
    if !(*cached_method).is_null() {
        return false;
    }
    let mut class_scratch = [0u8; MAX_NAME];
    let mut method_scratch = [0u8; MAX_NAME];
    let mut signature_scratch = [0u8; MAX_NAME];
    let (Some(class_len), Some(method_len), Some(signature_len)) = (
        class_name.decode(&mut class_scratch),
        method_name.decode(&mut method_scratch),
        signature.decode(&mut signature_scratch),
    ) else {
        return name_too_long();
    };
    resolve_method_plain(
        env,
        cached_class,
        cached_method,
        is_static,
        &class_scratch[..class_len],
        &method_scratch[..method_len],
        &signature_scratch[..signature_len],
    )
}

/// The body of [`resolve_field`], for plaintext names.
#[allow(clippy::too_many_arguments)]
unsafe fn resolve_field_plain(
    env: Env,
    cached_class: *mut jclass,
    cached_field: *mut jfieldID,
    is_static: bool,
    class_name: &[u8],
    field_name: &[u8],
    signature: &[u8],
) -> bool {
    if resolve_class_plain(env, cached_class, class_name) {
        return true;
    }

    let key = (class_name.to_vec(), field_name.to_vec(), signature.to_vec());
    if let Some(found) = lock(&FIELD_CACHE).get(&key).copied() {
        *cached_field = found as jfieldID;
        return false;
    }

    let field = with_cstr(field_name, |n| {
        with_cstr(signature, |s| {
            if is_static {
                jni_fn!(env, GetStaticFieldID)(env, *cached_class, n, s)
            } else {
                jni_fn!(env, GetFieldID)(env, *cached_class, n, s)
            }
        })
    });

    if field.is_null() {
        return true;
    }
    lock(&FIELD_CACHE).insert(key, field as usize);
    *cached_field = field;
    false
}

/// `d2c_resolve_field` — returns true when an exception is pending. See
/// [`resolve_class`] for what `class_name`/`field_name`/`signature` may be.
#[allow(clippy::too_many_arguments)]
pub unsafe fn resolve_field<C: NameArg, F: NameArg, S: NameArg>(
    env: Env,
    cached_class: *mut jclass,
    cached_field: *mut jfieldID,
    is_static: bool,
    class_name: C,
    field_name: F,
    signature: S,
) -> bool {
    if !(*cached_field).is_null() {
        return false;
    }
    let mut class_scratch = [0u8; MAX_NAME];
    let mut field_scratch = [0u8; MAX_NAME];
    let mut signature_scratch = [0u8; MAX_NAME];
    let (Some(class_len), Some(field_len), Some(signature_len)) = (
        class_name.decode(&mut class_scratch),
        field_name.decode(&mut field_scratch),
        signature.decode(&mut signature_scratch),
    ) else {
        return name_too_long();
    };
    resolve_field_plain(
        env,
        cached_class,
        cached_field,
        is_static,
        &class_scratch[..class_len],
        &field_scratch[..field_len],
        &signature_scratch[..signature_len],
    )
}

/// Register the natives of one class (`dynamic_register_compile_methods`).
///
/// `name` is a plain class path or, under `--obfuscate`, a [`Name`] -- a
/// `--dynamic-register` build registers by class name, so that name is as
/// identifying as any other reflected one and is encrypted by the same pass.
///
/// The entries are plain [`JNINativeMethod`]s, which is what a plain build emits.
/// An `--obfuscate` build emits [`ObfMethod`]s and calls
/// [`register_natives_obf`] instead, because the signature in each entry spells
/// out the parameter and return classes.
pub unsafe fn register_natives<C: NameArg>(env: Env, name: C, methods: &[JNINativeMethod]) -> bool {
    let mut scratch = [0u8; MAX_NAME];
    let Some(len) = name.decode(&mut scratch) else {
        return false;
    };
    let clz = find_class(env, &scratch[..len]);
    if clz.is_null() {
        return false;
    }
    let result =
        jni_fn!(env, RegisterNatives)(env, clz, methods.as_ptr(), methods.len() as jint) == 0;
    delete_local_ref(env, clz);
    result
}

/// Helper for the tables the generated `register_natives` builds.
///
/// `RegisterNatives` wants `const char*`, so the generated tables pass C string
/// literals (`c"add"`) — Rust's `b"add"` has no NUL terminator. `fn_ptr` is an
/// entry point such as `methods::Java_com_example_Hello_add__II as *const ()`.
pub fn native_method(
    name: &'static core::ffi::CStr,
    signature: &'static core::ffi::CStr,
    fn_ptr: *const (),
) -> JNINativeMethod {
    JNINativeMethod {
        name: name.as_ptr() as *mut core::ffi::c_char,
        signature: signature.as_ptr() as *mut core::ffi::c_char,
        fnPtr: fn_ptr as *mut core::ffi::c_void,
    }
}

/// One entry of an `--obfuscate` `RegisterNatives` table.
///
/// ART builds the call from the *signature*, so the table has to hand it the real
/// JNI descriptor — and that descriptor spells out the parameter and return
/// classes, which on an obfuscated app are the app's own (`(Lb0/d0;)V`).
/// Encrypting the class a table registers against while leaving its signatures
/// readable would leave behind exactly the names the pass exists to hide, and it
/// did: `tools/verify.py` found 182 app class names in a `--obfuscate
/// --dynamic-register` build before this. Both are stored as ciphertext here and
/// decoded at registration time instead.
///
/// A separate type, rather than the same [`JNINativeMethod`], because that one
/// holds `*const c_char` and has to be buildable in a `const` initialiser, which a
/// runtime decode is not. The decoded strings are owned by
/// [`register_natives_obf`] for the length of the registration call and released
/// right after it — ART reads the array during the call and keeps only the
/// function pointers.
#[derive(Clone, Copy)]
pub struct ObfMethod {
    name: Name,
    signature: Name,
    entry: *const (),
}

/// Build one [`ObfMethod`]: the obfuscated counterpart of [`native_method`].
pub const fn native_method_obf(name: Name, signature: Name, entry: *const ()) -> ObfMethod {
    ObfMethod {
        name,
        signature,
        entry,
    }
}

/// Decode one name into an owned C string.
///
/// `None` for a name over [`MAX_NAME`] (unreachable from generated code, see
/// [`name_too_long`]) or one carrying an interior NUL, which a class, method or
/// signature never has.
fn cstring<N: NameArg>(name: N) -> Option<std::ffi::CString> {
    let mut scratch = [0u8; MAX_NAME];
    let len = name.decode(&mut scratch)?;
    std::ffi::CString::new(&scratch[..len]).ok()
}

/// Decode an encrypted table into the plain one ART takes.
///
/// Returns the decoded strings **and** the array that points into them; the caller
/// has to keep both until the registration call has returned. Splitting this out
/// of [`register_natives_obf`] is what makes the decode testable without a JVM --
/// the part that can be wrong (wrong bytes, wrong order, a dropped entry) is here.
pub fn decode_table(
    methods: &[ObfMethod],
) -> Option<(Vec<std::ffi::CString>, Vec<JNINativeMethod>)> {
    let mut owned: Vec<std::ffi::CString> = Vec::with_capacity(methods.len() * 2);
    let mut plain: Vec<JNINativeMethod> = Vec::with_capacity(methods.len());
    for method in methods {
        let (Some(name), Some(signature)) = (cstring(method.name), cstring(method.signature))
        else {
            return None;
        };
        plain.push(JNINativeMethod {
            name: name.as_ptr() as *mut core::ffi::c_char,
            signature: signature.as_ptr() as *mut core::ffi::c_char,
            fnPtr: method.entry as *mut core::ffi::c_void,
        });
        // `CString::as_ptr` points into the `CString`'s own allocation, so growing
        // this vector does not move what `plain` already points at.
        owned.push(name);
        owned.push(signature);
    }
    Some((owned, plain))
}

/// [`register_natives`] for a table whose method names and signatures are
/// encrypted.
///
/// Decodes the table into the `JNINativeMethod` array ART wants, calling it while
/// the decoded strings are still alive.
///
/// # Safety
///
/// Same contract as [`register_natives`]: `env` must be a valid JNI environment
/// on the calling thread, and each `entry` must be the entry point for the method
/// its name and signature describe, with a matching ABI.
pub unsafe fn register_natives_obf<C: NameArg>(env: Env, name: C, methods: &[ObfMethod]) -> bool {
    let Some((owned, plain)) = decode_table(methods) else {
        return false;
    };
    let result = register_natives(env, name, &plain);
    // `plain` points into `owned`, and ART has read it by the time the call above
    // returns, so this is where the decoded strings may go.
    drop(owned);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two name forms have to decode to the same bytes: that is what lets one
    /// `resolve_*` body serve a plain build and an `--obfuscate` build, and it is
    /// the runtime half of `obfuscated_names_use_the_runtime_keystream` in
    /// `dex2rs-core/tests/compile.rs`, which pins the generator's keystream to
    /// this one.
    #[test]
    fn both_name_forms_decode_to_the_same_bytes() {
        let plain: &[u8; 16] = b"java/lang/Object";
        let seed = 0x51c0_ffee;
        let keys = crate::keystream::<16>(seed);
        let cipher: Vec<u8> = plain.iter().zip(keys).map(|(b, k)| b ^ k).collect();
        let cipher: &'static [u8] = Box::leak(cipher.into_boxed_slice());

        let mut scratch = [0u8; MAX_NAME];
        let len = Name::obf(cipher, seed)
            .decode(&mut scratch)
            .expect("decodes");
        assert_eq!(&scratch[..len], &plain[..]);

        let mut plain_scratch = [0u8; MAX_NAME];
        let len = plain.decode(&mut plain_scratch).expect("copies");
        assert_eq!(&plain_scratch[..len], &plain[..]);
    }

    /// The register table has to decode to exactly what the plain build puts in
    /// the same slot: same names, same signatures, same order, same count.
    ///
    /// `--dynamic-register --obfuscate` is the only build that goes through this
    /// path, and a mistake here is invisible in the generated source -- it shows up
    /// on the device as a converted method that is never registered, i.e. an
    /// `UnsatisfiedLinkError` on an obfuscated build only.
    #[test]
    fn an_obfuscated_register_table_decodes_to_the_plain_one() {
        /// Long enough for the names and signatures below.
        const CAP: usize = 64;

        /// The ciphertext the generator would emit for `plain`, as a `'static`
        /// slice so it can go into a `Name`.
        fn cipher(plain: &[u8], seed: u32) -> &'static [u8] {
            let keys = crate::keystream::<CAP>(seed);
            let bytes: Vec<u8> = plain.iter().enumerate().map(|(i, b)| b ^ keys[i]).collect();
            Box::leak(bytes.into_boxed_slice())
        }

        let table = [
            native_method_obf(
                Name::obf(cipher(b"onCreate", 0x0bad_c0de), 0x0bad_c0de),
                Name::obf(cipher(b"(Landroid/os/Bundle;)V", 0x0fee_face), 0x0fee_face),
                core::ptr::null(),
            ),
            native_method_obf(
                Name::obf(cipher(b"c", 0x1111_2222), 0x1111_2222),
                Name::obf(cipher(b"(Lb0/d0;)V", 0x3333_4444), 0x3333_4444),
                core::ptr::null(),
            ),
        ];

        let (owned, plain) = decode_table(&table).expect("decodes");
        assert_eq!(
            owned.len(),
            table.len() * 2,
            "one C string per name and signature"
        );
        assert_eq!(plain.len(), table.len(), "one entry per method");

        // Read them back the way ART does, so the assertion is on the bytes the
        // pointers point at rather than on the decode's own idea of them.
        let text = |pointer: *mut core::ffi::c_char| unsafe {
            core::ffi::CStr::from_ptr(pointer as *const core::ffi::c_char)
                .to_bytes()
                .to_vec()
        };
        let expected: [(Vec<u8>, Vec<u8>); 2] = [
            (b"onCreate".to_vec(), b"(Landroid/os/Bundle;)V".to_vec()),
            (b"c".to_vec(), b"(Lb0/d0;)V".to_vec()),
        ];
        for (entry, (name, signature)) in plain.iter().zip(expected) {
            assert_eq!(text(entry.name), name);
            assert_eq!(text(entry.signature), signature);
        }
    }

    /// A name past the bound is refused rather than truncated: half a class name
    /// resolves to nothing and would look like a missing class.
    #[test]
    fn a_name_past_the_bound_is_refused() {
        let mut scratch = [0u8; MAX_NAME];
        let fits = [b'x'; MAX_NAME - 1];
        assert_eq!((&fits).decode(&mut scratch), Some(MAX_NAME - 1));

        let oversized = [b'x'; MAX_NAME];
        let keys = crate::keystream::<MAX_NAME>(1);
        let cipher: Vec<u8> = oversized.iter().zip(keys).map(|(b, k)| b ^ k).collect();
        let cipher: &'static [u8] = Box::leak(cipher.into_boxed_slice());
        assert_eq!(Name::obf(cipher, 1).decode(&mut scratch), None);
    }
}
