//! Array, field and invocation helpers — the Rust equivalent of the JNI calls
//! `dex2c/writer.py` emitted inline (`Get*Field`, `Set*ArrayRegion`,
//! `Call*MethodA`, ...).
//!
//! Everything is dispatched by the *descriptor byte* of the operand type
//! (`b'I'`, `b'J'`, `b'L'`, `b'['`, ...), which is exactly what
//! `util.get_type_descriptor()` returned to the C++ writer.

use jni_sys::*;

use crate::Env;

/// `Call<Type>MethodA`
pub const CALL_VIRTUAL: u8 = 0;
/// `CallNonvirtual<Type>MethodA` (the `invoke-super` case)
pub const CALL_NONVIRTUAL: u8 = 1;
/// `CallStatic<Type>MethodA`
pub const CALL_STATIC: u8 = 2;

// ---------------------------------------------------------------- arrays ------

/// `env->GetArrayLength`
#[inline]
pub unsafe fn array_length(env: Env, array: jarray) -> jsize {
    jni_fn!(env, GetArrayLength)(env, array)
}

/// `env->NewObjectArray(len, clz, NULL)`
#[inline]
pub unsafe fn new_object_array(env: Env, len: jsize, clz: jclass) -> jarray {
    jni_fn!(env, NewObjectArray)(env, len, clz, core::ptr::null_mut())
}

/// `env->GetObjectArrayElement`
#[inline]
pub unsafe fn get_object_array_elem(env: Env, array: jarray, idx: jsize) -> jobject {
    jni_fn!(env, GetObjectArrayElement)(env, array, idx)
}

/// `env->SetObjectArrayElement`
#[inline]
pub unsafe fn set_object_array_elem(env: Env, array: jarray, idx: jsize, value: jobject) {
    jni_fn!(env, SetObjectArrayElement)(env, array, idx, value);
}

/// `d2c_filled_new_array` / `fill-array-data` for object arrays.
#[inline]
pub unsafe fn set_object_array_region(env: Env, array: jarray, values: &[jobject]) {
    for (i, value) in values.iter().enumerate() {
        jni_fn!(env, SetObjectArrayElement)(env, array, i as jsize, *value);
    }
}

// The generated code keeps every int-like register in one `jint` slot (exactly
// what the C++ writer did: `cast_to` on `dex2c/writer.py` only widened J/F/D, and
// every `jvalue` union assignment truncated implicitly). The accessors below are
// therefore named after the JNI entry points but take the *register* type and
// narrow at the boundary, so `SetIntField(env, obj, fid, v)` is the only shape
// the emitter has to produce.

/// Single-element `Get<Type>ArrayRegion(array, index)` (the emitter's shape),
/// plus instance/static field access, per primitive type.
macro_rules! primitive_accessors {
    ($( $get:ident, $set:ident, $new:ident, $jty:ty, $inty:ty, $conv:ident,
        $fget:ident, $fset:ident, $sfget:ident, $sfset:ident ),* $(,)?) => {
        $(
            #[inline]
            pub unsafe fn $new(env: Env, len: jsize) -> jarray {
                jni_fn!(env, $new)(env, len)
            }

            /// `array[index]`.
            #[inline]
            pub unsafe fn $get(env: Env, array: jarray, idx: jsize) -> $jty {
                let mut value: $jty = Default::default();
                jni_fn!(env, $get)(env, array, idx, 1, &mut value);
                value
            }

            /// `array[index] = value`.
            #[inline]
            pub unsafe fn $set(env: Env, array: jarray, idx: jsize, value: $inty) {
                let value: $jty = ($conv)(value);
                jni_fn!(env, $set)(env, array, idx, 1, &value);
            }

            #[inline]
            pub unsafe fn $fget(env: Env, obj: jobject, fid: jfieldID) -> $jty {
                jni_fn!(env, $fget)(env, obj, fid)
            }

            #[inline]
            pub unsafe fn $fset(env: Env, obj: jobject, fid: jfieldID, value: $inty) {
                jni_fn!(env, $fset)(env, obj, fid, ($conv)(value));
            }

            #[inline]
            pub unsafe fn $sfget(env: Env, clz: jclass, fid: jfieldID) -> $jty {
                jni_fn!(env, $sfget)(env, clz, fid)
            }

            #[inline]
            pub unsafe fn $sfset(env: Env, clz: jclass, fid: jfieldID, value: $inty) {
                jni_fn!(env, $sfset)(env, clz, fid, ($conv)(value));
            }
        )*
    };
}

#[inline]
fn to_jboolean(v: jint) -> jboolean {
    v != 0
}
#[inline]
fn to_jbyte(v: jint) -> jbyte {
    v as jbyte
}
#[inline]
fn to_jchar(v: jint) -> jchar {
    v as jchar
}
#[inline]
fn to_jshort(v: jint) -> jshort {
    v as jshort
}
#[inline]
fn to_jint(v: jint) -> jint {
    v
}
#[inline]
fn to_jlong(v: jlong) -> jlong {
    v
}
#[inline]
fn to_jfloat(v: jfloat) -> jfloat {
    v
}
#[inline]
fn to_jdouble(v: jdouble) -> jdouble {
    v
}

primitive_accessors! {
    GetBooleanArrayRegion, SetBooleanArrayRegion, NewBooleanArray, jboolean, jint, to_jboolean,
        GetBooleanField, SetBooleanField, GetStaticBooleanField, SetStaticBooleanField,
    GetByteArrayRegion, SetByteArrayRegion, NewByteArray, jbyte, jint, to_jbyte,
        GetByteField, SetByteField, GetStaticByteField, SetStaticByteField,
    GetCharArrayRegion, SetCharArrayRegion, NewCharArray, jchar, jint, to_jchar,
        GetCharField, SetCharField, GetStaticCharField, SetStaticCharField,
    GetShortArrayRegion, SetShortArrayRegion, NewShortArray, jshort, jint, to_jshort,
        GetShortField, SetShortField, GetStaticShortField, SetStaticShortField,
    GetIntArrayRegion, SetIntArrayRegion, NewIntArray, jint, jint, to_jint,
        GetIntField, SetIntField, GetStaticIntField, SetStaticIntField,
    GetLongArrayRegion, SetLongArrayRegion, NewLongArray, jlong, jlong, to_jlong,
        GetLongField, SetLongField, GetStaticLongField, SetStaticLongField,
    GetFloatArrayRegion, SetFloatArrayRegion, NewFloatArray, jfloat, jfloat, to_jfloat,
        GetFloatField, SetFloatField, GetStaticFloatField, SetStaticFloatField,
    GetDoubleArrayRegion, SetDoubleArrayRegion, NewDoubleArray, jdouble, jdouble, to_jdouble,
        GetDoubleField, SetDoubleField, GetStaticDoubleField, SetStaticDoubleField,
}

/// `env->Set<Type>ArrayRegion(array, 0, n, data)` for the whole array — used by
/// both `filled-new-array` and `fill-array-data`.
macro_rules! fill_array_macros {
    ($( $name:ident, $set:ident, $jty:ty ),* $(,)?) => {
        $(
            #[doc = concat!("Write `values` from the start of an array of `", stringify!($jty), "`.")]
            #[inline]
            pub unsafe fn $name(env: Env, array: jarray, values: &[$jty]) {
                jni_fn!(env, $set)(env, array, 0, values.len() as jsize, values.as_ptr());
            }
        )*
    };
}

fill_array_macros! {
    set_boolean_array, SetBooleanArrayRegion, jboolean,
    set_byte_array, SetByteArrayRegion, jbyte,
    set_char_array, SetCharArrayRegion, jchar,
    set_short_array, SetShortArrayRegion, jshort,
    set_int_array, SetIntArrayRegion, jint,
    set_long_array, SetLongArrayRegion, jlong,
    set_float_array, SetFloatArrayRegion, jfloat,
    set_double_array, SetDoubleArrayRegion, jdouble,
}

// ------------------------------------------------------------ object fields ---

/// `env->GetObjectField`
#[inline]
pub unsafe fn get_object_field(env: Env, obj: jobject, fid: jfieldID) -> jobject {
    jni_fn!(env, GetObjectField)(env, obj, fid)
}

/// `env->SetObjectField`
#[inline]
pub unsafe fn set_object_field(env: Env, obj: jobject, fid: jfieldID, value: jobject) {
    jni_fn!(env, SetObjectField)(env, obj, fid, value);
}

/// `env->GetStaticObjectField`
#[inline]
pub unsafe fn get_static_object_field(env: Env, clz: jclass, fid: jfieldID) -> jobject {
    jni_fn!(env, GetStaticObjectField)(env, clz, fid)
}

/// `env->SetStaticObjectField`
#[inline]
pub unsafe fn set_static_object_field(env: Env, clz: jclass, fid: jfieldID, value: jobject) {
    jni_fn!(env, SetStaticObjectField)(env, clz, fid, value);
}

// ---------------------------------------------------------------- calls -------

/// `CallVoidMethodA` and friends (the result is ignored).
#[inline]
pub unsafe fn call_void_method(
    env: Env,
    call: u8,
    obj: jobject,
    clz: jclass,
    mid: jmethodID,
    args: *const jvalue,
) {
    match call {
        CALL_NONVIRTUAL => jni_fn!(env, CallNonvirtualVoidMethodA)(env, obj, clz, mid, args),
        CALL_STATIC => jni_fn!(env, CallStaticVoidMethodA)(env, clz, mid, args),
        _ => jni_fn!(env, CallVoidMethodA)(env, obj, mid, args),
    }
}

macro_rules! call_macros {
    ($( $name:ident, $jty:ty, $virt:ident, $nonvirt:ident, $static:ident ),* $(,)?) => {
        $(
            #[doc = concat!("`Call", stringify!($virt), "` / nonvirtual / static dispatch.")]
            #[inline]
            pub unsafe fn $name(
                env: Env,
                call: u8,
                obj: jobject,
                clz: jclass,
                mid: jmethodID,
                args: *const jvalue,
            ) -> $jty {
                match call {
                    CALL_NONVIRTUAL => jni_fn!(env, $nonvirt)(env, obj, clz, mid, args),
                    CALL_STATIC => jni_fn!(env, $static)(env, clz, mid, args),
                    _ => jni_fn!(env, $virt)(env, obj, mid, args),
                }
            }
        )*
    };
}

call_macros! {
    call_boolean_method, jboolean, CallBooleanMethodA, CallNonvirtualBooleanMethodA,
        CallStaticBooleanMethodA,
    call_byte_method, jbyte, CallByteMethodA, CallNonvirtualByteMethodA, CallStaticByteMethodA,
    call_char_method, jchar, CallCharMethodA, CallNonvirtualCharMethodA, CallStaticCharMethodA,
    call_short_method, jshort, CallShortMethodA, CallNonvirtualShortMethodA, CallStaticShortMethodA,
    call_int_method, jint, CallIntMethodA, CallNonvirtualIntMethodA, CallStaticIntMethodA,
    call_long_method, jlong, CallLongMethodA, CallNonvirtualLongMethodA, CallStaticLongMethodA,
    call_float_method, jfloat, CallFloatMethodA, CallNonvirtualFloatMethodA, CallStaticFloatMethodA,
    call_double_method, jdouble, CallDoubleMethodA, CallNonvirtualDoubleMethodA,
        CallStaticDoubleMethodA,
    call_object_method, jobject, CallObjectMethodA, CallNonvirtualObjectMethodA,
        CallStaticObjectMethodA,
}
