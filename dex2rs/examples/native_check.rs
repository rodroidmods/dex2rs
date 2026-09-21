//! List the JNI symbols a converted APK's native methods expect.
//!
//! `cargo run -q -p dex2rs --example native_check -- out.apk > expected.txt`
//! then compare with `llvm-nm --defined-only -D libstub.so | awk '{print $3}'`.
//!
//! Any missing symbol is an `UnsatisfiedLinkError` on device.

use std::path::Path;

use dex2rs::apk;
use smali::types::Modifier;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "output.apk".to_string());
    let input = apk::load(Path::new(&path))?;
    let mut count = 0usize;
    for unit in input.dexes() {
        for class in &unit.classes {
            let cls = class.name.as_jni_type();
            for method in &class.methods {
                if !method.modifiers.contains(&Modifier::Native) {
                    continue;
                }
                count += 1;
                println!(
                    "{}",
                    dex2rs_core::util::jni_long_name(
                        &cls,
                        &method.name,
                        &method.signature.to_jni()
                    )
                );
            }
        }
    }
    eprintln!("{count} native methods");
    Ok(())
}
