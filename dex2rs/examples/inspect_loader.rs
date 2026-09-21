//! Print what `inject_loader` does to a real APK: the app class, the injected
//! `<clinit>`, and whether the patched class still assembles.
//!
//! Usage: cargo run -q --example inspect_loader -- app-release.apk stub

use std::collections::BTreeSet;

use dex2rs::apk;
use smali::dex::DexFile;
use smali::types::{Modifier, SmaliClass};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "app-release.apk".to_string());
    let lib = args.next().unwrap_or_else(|| "stub".to_string());
    let loader = "amimo.dcc.DccApplication".to_string();

    let mut input = apk::load(std::path::Path::new(&path))?;
    let (app_class, min_sdk) = match &input {
        apk::Input::Apk {
            app_class, min_sdk, ..
        } => (app_class.clone(), *min_sdk),
        _ => (None, 0),
    };
    println!("app class  : {app_class:?}");
    println!("min sdk    : {min_sdk}");
    println!(
        "dexes      : {}",
        input
            .dexes()
            .iter()
            .map(|d| format!(
                "{} ({} classes, v{:03})",
                d.name,
                d.classes.len(),
                d.version
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Does the app class actually exist in the dexes, and what does its <clinit>
    // look like right now?
    if let Some(app) = &app_class {
        for unit in input.dexes() {
            if let Some(class) = unit.classes.iter().find(|c| c.name.as_java_type() == *app) {
                let clinit = class
                    .methods
                    .iter()
                    .find(|m| m.name == "<clinit>")
                    .map(|m| (m.modifiers.clone(), m.locals, m.registers));
                println!("found in   : {}", unit.name);
                println!("<clinit>   : {clinit:?}");
                let rendered = format!("{class}");
                let reparsed = SmaliClass::from_smali(&rendered);
                println!("reparse ok : {}", reparsed.is_ok());
                if let Err(error) = reparsed {
                    println!("reparse err: {error}");
                }
                break;
            }
        }
    }

    // Nothing is compiled: we only want the loader rewrite.
    let empty: BTreeSet<(String, String, String)> = BTreeSet::new();
    apk::patch_compiled_methods(input.dexes_mut(), &empty, true);
    apk::inject_loader(&mut input, &lib, &loader, false)?;

    let Some(app) = &app_class else {
        println!("no application class; loader class was appended");
        return Ok(());
    };
    for unit in input.dexes() {
        let Some(class) = unit.classes.iter().find(|c| c.name.as_java_type() == *app) else {
            continue;
        };
        let clinits: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name == "<clinit>")
            .collect();
        println!("\n<clinit> count after injection: {}", clinits.len());
        for m in clinits {
            println!("  modifiers={:?} locals={:?}", m.modifiers, m.locals);
            println!("  ops={:?}", m.ops);
        }
        let rendered = format!("{class}");
        println!(
            "\n--- patched class smali (first 2500 bytes) ---\n{}",
            &rendered[..rendered.len().min(2500)]
        );
        println!(
            "\nreassembles: {}",
            DexFile::from_smali(std::slice::from_ref(class)).is_ok()
        );
        let has_load = class
            .methods
            .iter()
            .any(|m| format!("{:?}", m.ops).contains("loadLibrary"));
        println!("calls loadLibrary: {has_load}");
        let _ = Modifier::Native;
        break;
    }
    Ok(())
}
