//! JNI entry point for the converted library.
//!
//! This is the Rust equivalent of `jni/nc/Dex2C.cpp`'s `JNI_OnLoad`: grab the
//! `JNIEnv`, cache the well-known classes the runtime resolves lazily, then
//! either rely on the exported `Java_*` symbols (default) or run the generated
//! `RegisterNatives` table (`--dynamic-register`).

use dex2rs_runtime as d2c;

mod dynamic_register;

// Public so the `#[no_mangle]` entry points inside stay reachable and therefore
// exported from the cdylib.
pub mod methods;

#[unsafe(no_mangle)]
pub unsafe extern "system" fn JNI_OnLoad(
    vm: *mut d2c::jni_sys::JavaVM,
    _reserved: *mut core::ffi::c_void,
) -> d2c::jni_sys::jint {
    use d2c::jni_sys::{JNI_ERR, JNI_OK, JNI_VERSION_1_6};

    unsafe {
        let mut raw_env: *mut core::ffi::c_void = core::ptr::null_mut();
        // jni-sys models the invoke interface as a union of versioned views;
        // `GetEnv` is part of the 1.2 view.
        let get_env = (*(*vm)).v1_2.GetEnv;
        if get_env(vm, &mut raw_env, JNI_VERSION_1_6) != JNI_OK {
            return JNI_ERR;
        }
        let env = raw_env as d2c::Env;

        if !d2c::init(env) {
            d2c::log_error("dex2rs: failed to cache well-known classes");
            return JNI_ERR;
        }

        if let Err(error) = dynamic_register::register(env) {
            d2c::log_error(&error);
            return JNI_ERR;
        }

        JNI_VERSION_1_6
    }
}
